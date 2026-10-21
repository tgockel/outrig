//! The host's proxy variables reach a container only when something names
//! them (#455). This exports a proxy URL carrying a password and reads back
//! what the containers hold.
//!
//! A binary of its own because it sets those variables in the process
//! environment, which every `podman` this binary spawns inherits.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test host_proxy_e2e -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use outrig::ExecOptions;
use outrig::config::{EnvValue, ResolvedEnvValue};
use outrig::container::{Container, ContainerCreateOptions, ContainerLaunchSpec};
use outrig::image::ImageTag;

const PROXY: &str = "http://outrig-user:outrig-secret@proxy.invalid:3128";

/// Fails naming each `KEY=value` line of `env` that is a proxy variable, in
/// either case, or that carries the proxy's password under any other name.
fn assert_no_proxy(what: &str, env: &str) {
    let leaked: Vec<&str> = env
        .lines()
        .filter(|entry| {
            let key = entry.split('=').next().unwrap_or_default();
            key.to_ascii_uppercase().ends_with("_PROXY") || entry.contains("outrig-secret")
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "{what} was handed the host's proxy: {leaked:?}"
    );
}

/// Stdout of `argv`, exec'd in `container` the way an MCP server is.
async fn exec_stdout(container: &Container, argv: &[&str], options: &ExecOptions) -> String {
    let argv: Vec<String> = argv.iter().map(|s| (*s).to_string()).collect();
    let mut child = container
        .exec_stdio(&argv, options)
        .await
        .expect("exec_stdio");
    common::read_stdout(&mut child).await
}

#[tokio::test]
async fn a_host_proxy_reaches_a_container_only_when_named() {
    common::init_tracing();
    // Before the variables are set: a pull would go through the proxy.
    common::pull_alpine();
    // SAFETY: edition 2024 marks `env::set_var` unsafe because of multi-thread
    // races. This is the only test in this binary, and nothing has been
    // spawned on its runtime yet, so no thread can read the environment while
    // this writes it.
    unsafe {
        std::env::set_var("HTTPS_PROXY", PROXY);
        std::env::set_var("http_proxy", PROXY);
        std::env::set_var("NO_PROXY", "localhost,127.0.0.1");
    }

    // A primary, the way a session starts one: `podman run`.
    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut primary = common::start_alpine(host_ws.path(), None).await;
    primary.bootstrap_user().await.expect("bootstrap_user");
    let env = exec_stdout(&primary, &["env"], &ExecOptions::new()).await;
    assert_no_proxy("an exec in the primary", &env);

    // Named, it arrives whole: the route an MCP server's
    // `env = { HTTPS_PROXY = "${HTTPS_PROXY}" }` takes.
    let named = ResolvedEnvValue::resolve(EnvValue::EnvRef("HTTPS_PROXY".to_string()))
        .expect("HTTPS_PROXY is set");
    let options =
        ExecOptions::new().with_resolved_env(BTreeMap::from([("HTTPS_PROXY".to_string(), named)]));
    let named = exec_stdout(
        &primary,
        &["sh", "-c", "printf %s \"$HTTPS_PROXY\""],
        &options,
    )
    .await;
    assert_eq!(named, PROXY, "a proxy the config names has to reach it");

    // An entrypoint container, the way a sidecar gets one: `podman create`.
    let name = format!("outrig-test-host-proxy-{}", std::process::id());
    let created = Container::create_initialized(ContainerCreateOptions::new(
        ImageTag::new(common::ALPINE),
        ContainerLaunchSpec::default(),
        &name,
    ))
    .await
    .expect("create_initialized");
    let inspected = common::run_capture(Command::new("podman").args([
        "inspect",
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
        &name,
    ]));
    assert_no_proxy(
        "a created container",
        &String::from_utf8_lossy(&inspected.stdout),
    );

    created.stop(Duration::from_secs(2)).await.expect("stop");
    primary.stop(Duration::from_secs(2)).await.expect("stop");
}
