//! End-to-end tests that what `outrig image add` generates works. Gated
//! behind `--features e2e` because they shell out to a real `buildah` and
//! `podman` and pull real base images.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p outrig-cli --features e2e --test image_add_buildable -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::path::Path;
use std::time::Duration;

use tokio::time::timeout;

use outrig::ExecOptions;
use outrig::config::Config;
use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image;
use outrig_cli::image_setup::add::run_with;
use outrig_cli::image_setup::render::{self, BaseImage, Toolchain};

use common::scripted_prompt;

const TEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Uses the smallest viable combination -- alpine base, no toolchains, fs MCP
/// server -- so the e2e cost stays low.
#[tokio::test]
async fn generated_alpine_image_builds() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let tmp = tempfile::tempdir().expect("tempdir");
    let cfg_dir = tmp.path().join(".agents/outrig");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("config.toml"), "").unwrap();

    // Script: base = alpine:latest, toolchains = [], MCPs = [fs] (default).
    let (mut prompt, _stderr) = scripted_prompt(b"alpine:latest\n\n\n").await;

    timeout(
        TEST_TIMEOUT,
        run_with(tmp.path(), Some("coding".to_string()), false, &mut prompt),
    )
    .await
    .expect("run_with must not hang")
    .expect("run_with must succeed");

    let cfg_text = std::fs::read_to_string(tmp.path().join(".agents/outrig/config.toml")).unwrap();
    let cfg = Config::load_from_str(&cfg_text).expect("config must parse");
    let image = cfg.images.get("coding").expect("coding image present");

    let outcome = image::ensure_image(image, tmp.path(), false)
        .await
        .expect("ensure_image must succeed against generated config");
    assert!(
        !outcome.tag.as_str().is_empty(),
        "image tag must not be empty"
    );
}

/// A toolchain's download and install take minutes.
const TOOLCHAIN_BUILD_TIMEOUT: Duration = Duration::from_secs(900);

/// Build what `image add` renders for `toolchain` on the default base, start
/// it the way a session does, and run `probe` as the user OutRig runs every
/// exec as: the host UID, with `HOME=/home/<user>`. The probe must succeed
/// with each of `expected` in its stdout.
async fn assert_toolchain_probe(toolchain: Toolchain, probe: &str, expected: &[&str]) {
    let project = tempfile::tempdir().expect("tempdir");
    let dockerfile = render::render(BaseImage::DebianBookwormSlim, &[toolchain], &[]);
    std::fs::write(project.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");

    let tag = timeout(
        TOOLCHAIN_BUILD_TIMEOUT,
        image::ensure_image(&common::fixture_build_config(), project.path(), false),
    )
    .await
    .expect("the toolchain image build must not hang")
    .expect("the generated toolchain Dockerfile must build")
    .tag;

    // Not under a timeout: the first `--userns=keep-id` start of an image this
    // size can spend minutes remapping its layers (#284).
    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = Container::start(
        &tag,
        ContainerLaunchSpec::workspace(host_ws.path(), Path::new("/workspace")),
    )
    .await
    .expect("Container::start");
    container.bootstrap_user().await.expect("bootstrap_user");

    // `exec_capture` builds the same `podman exec` an exec-stdio MCP server
    // gets: the session's `--user` and `HOME`.
    let argv = ["sh", "-c", probe].map(String::from);
    let out = timeout(
        TEST_TIMEOUT,
        container.exec_capture(&argv, &ExecOptions::new()),
    )
    .await;
    // Stopped before anything is asserted, so a failure does not leave the
    // container to `Drop`'s detached removal.
    container.stop(Duration::from_secs(2)).await.expect("stop");

    let out = out
        .expect("the toolchain probe must not hang")
        .expect("exec_capture");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && expected.iter().all(|e| stdout.contains(e)),
        "the session user cannot use the generated {} toolchain; expected {expected:?} in \
         its stdout ({})\n--- stdout ---\n{stdout}--- stderr ---\n{stderr}",
        toolchain.as_str(),
        out.status,
    );
}

/// Run the toolchain and the components `image add` advertises, write where
/// cargo and rustup keep their state, then build and link a crate.
const RUST_PROBE: &str = r#"set -eux
id
cargo --version
rustc --version
cargo fmt --version
cargo clippy --version
touch "$CARGO_HOME/.outrig-probe" "$RUSTUP_HOME/.outrig-probe"
cd "$HOME"
cargo new --vcs none --quiet probe
cd probe
cargo run --offline --quiet
"#;

/// The `rust` toolchain works for the user OutRig runs every exec as: the host
/// UID, with `HOME=/home/<user>`. It was installed into `/root`, which that
/// user cannot enter, and with nothing to tell rustup where its toolchains
/// were once `HOME` had moved -- and the image built cleanly all the same
/// (#183).
#[tokio::test]
async fn generated_rust_toolchain_runs_as_the_session_user() {
    common::init_tracing();
    assert_toolchain_probe(Toolchain::Rust, RUST_PROBE, &["Hello, world!"]).await;
}

/// Print the platform the toolchain was built for, then build and run a
/// program.
const GO_PROBE: &str = r#"set -eux
id
go version
cd "$HOME"
cat > hello.go <<'EOF'
package main

import "fmt"

func main() { fmt.Println("Hello, world!") }
EOF
go run hello.go
"#;

/// The `go` toolchain is the one for the host's architecture, and works for
/// the session user. It was always the x86-64 archive, which on an AArch64
/// host built cleanly and then failed every `go` with `exec format error`
/// (#185). CI's `live-e2e (aarch64)` row is what runs this there.
#[tokio::test]
async fn generated_go_toolchain_runs_on_the_host_architecture() {
    common::init_tracing();
    let goarch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("the generated go toolchain does not support {other}"),
    };
    let platform = format!("linux/{goarch}");
    assert_toolchain_probe(Toolchain::Go, GO_PROBE, &[&platform, "Hello, world!"]).await;
}

/// Print what the toolchain installed, as the session user.
const NODE_PROBE: &str = r#"set -eux
id
node --version
npm --version
"#;

/// The `node` toolchain is NodeSource's Node, with the `npm` its package
/// ships, for the session user. `v24.` moves with `node_major` in
/// `templates/node.debian.dockerfile`.
#[tokio::test]
async fn generated_node_toolchain_is_nodesources_node() {
    common::init_tracing();
    assert_toolchain_probe(Toolchain::Node, NODE_PROBE, &["v24."]).await;
}

/// A `curl` ahead of the real one on `PATH` that cannot fetch NodeSource's
/// signing key, the way a transient network failure would leave it. The setup
/// script's own `apt update` has run by then.
const KEY_FETCH_FAILS: &str = r#"RUN printf '#!/bin/sh\ncase "$*" in *gpgkey*) exit 22 ;; esac\nexec /usr/bin/curl "$@"\n' \
      > /usr/local/bin/curl \
 && chmod +x /usr/local/bin/curl
"#;

/// NodeSource's setup script exits 0 whatever goes wrong in it, so a `node`
/// toolchain whose setup failed used to build anyway: `apt-get install nodejs`
/// took Debian's Node 18, which has no `npm`, and the image was cached under
/// its normal tag (#565). The build must fail, saying which Node it found.
#[tokio::test]
async fn a_node_toolchain_whose_nodesource_setup_failed_does_not_build() {
    common::init_tracing();
    let dockerfile = render::render(BaseImage::DebianBookwormSlim, &[Toolchain::Node], &[]);
    let (header, node) = dockerfile
        .split_once("\n# node toolchain\n")
        .expect("a node toolchain section");
    let dockerfile = format!("{header}\n{KEY_FETCH_FAILS}\n# node toolchain\n{node}");

    let project = tempfile::tempdir().expect("tempdir");
    std::fs::write(project.path().join("Dockerfile"), dockerfile).expect("write Dockerfile");
    // Outside the build context, which the image's tag is a hash of.
    let logs = tempfile::tempdir().expect("tempdir");
    let log = logs.path().join("build.log");
    let transcript = outrig::Transcript::create(&log, false)
        .await
        .expect("create transcript");

    let image = "outrig-e2e-node-setup";
    let cfg = common::fixture_build_config();
    let tag = image::compute_tag_for(image, &cfg, project.path())
        .await
        .expect("compute tag");
    let built = timeout(
        TOOLCHAIN_BUILD_TIMEOUT,
        image::ensure_tagged_image_for(image, &cfg, project.path(), &tag, false, Some(&transcript)),
    )
    .await
    .expect("the toolchain image build must not hang");

    let log = std::fs::read_to_string(&log).expect("read the build log");
    let err = built.expect_err(&format!(
        "a node toolchain whose NodeSource setup failed built an image\n--- build log ---\n{log}"
    ));
    // Buildah echoes each `RUN` before it runs it, so the log holds the check's
    // `echo` as written -- `$node_major` and all -- even when the build failed
    // before the check ran. Only the expanded message came from the check.
    // `v18.` is the `nodejs` in Debian's own archive; `24` moves with
    // `node_major`, like the probe's `v24.`.
    assert!(
        log.contains("node toolchain: expected Node 24, found v18."),
        "the build failed, but not at the node toolchain's check: {err}\n--- build log ---\n{log}",
    );
}
