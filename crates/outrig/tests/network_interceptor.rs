//! End-to-end smoke for audit-mode network interception. Gated behind
//! `--features e2e` because it needs podman/buildah and nftables namespace
//! access.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test network_interceptor -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

use outrig::config::{ImageConfig, NetworkAction, NetworkPolicy};
use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::{self, ImageTag};
use outrig::network::NetworkInterceptor;

static E2E_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn write_curl_image_context(dir: &Path) {
    std::fs::write(
        dir.join("Dockerfile"),
        r#"
FROM docker.io/library/alpine:latest
RUN apk add --no-cache ca-certificates curl shadow
"#,
    )
    .expect("write Dockerfile");
}

async fn ensure_curl_image(context: &Path) -> ImageTag {
    let cfg = ImageConfig {
        image_name: None,
        dockerfile: Some("Dockerfile".into()),
        context: Some(".".into()),
        build_args: BTreeMap::new(),
        security: Default::default(),
        mcp: BTreeMap::new(),
    };
    image::ensure_image(&cfg, context, false)
        .await
        .expect("ensure curl image")
        .tag
}

async fn read_audit_records(log_dir: &Path) -> Vec<Value> {
    let path = log_dir.join("network.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let records: Vec<Value> = text
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| serde_json::from_str(line).expect("valid audit JSON"))
                .collect();
            if !records.is_empty() {
                return records;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn run_capture(cmd: &mut Command) -> Output {
    let output = cmd.output().expect("spawn command");
    assert!(
        output.status.success(),
        "command exited non-zero: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn try_capture(cmd: &mut Command) -> Output {
    cmd.output().expect("spawn command")
}

fn container_host_ipv4(container: &Container) -> String {
    let output = run_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["getent", "hosts", "host.containers.internal"]),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let ip = fields.next()?;
            ip.parse::<Ipv4Addr>().is_ok().then(|| ip.to_string())
        })
        .unwrap_or_else(|| panic!("host.containers.internal resolved to no IPv4 address: {stdout}"))
}

fn start_http_fixture() -> (SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind fixture HTTP server");
    listener
        .set_nonblocking(true)
        .expect("fixture listener nonblocking");
    let addr = listener.local_addr().expect("fixture listener addr");
    let handle = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buf = [0u8; 4096];
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Length: 2\r\n\
                          Connection: close\r\n\
                          \r\n\
                          ok",
                    );
                    return;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return,
            }
        }
    });
    (addr, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curl_http_host_writes_allow_audit_record() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let mut container = Container::start(
        &image,
        ContainerLaunchSpec::workspace(workspace.path(), PathBuf::from("/workspace")),
    )
    .await
    .expect("start container");
    container.bootstrap_user().await.expect("bootstrap user");
    let host_ip = container_host_ipv4(&container);
    let (server_addr, server_handle) = start_http_fixture();

    let interceptor = NetworkInterceptor::start(&container, &log_dir, container.session_suffix())
        .await
        .expect("start network interceptor");

    let output = try_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["curl", "-fsS", "--connect-timeout", "5", "--max-time", "10"])
            .args(["-H", "Host: example.com"])
            .arg(format!("http://{host_ip}:{}/", server_addr.port())),
    );
    let _ = server_handle.join();
    assert!(
        output.status.success(),
        "curl through interceptor failed: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let records = read_audit_records(&log_dir).await;
    let record = records
        .iter()
        .find(|record| {
            record.get("outrig.host").and_then(Value::as_str) == Some("example.com")
                && record.get("id.resp_p").and_then(Value::as_u64)
                    == Some(server_addr.port() as u64)
        })
        .unwrap_or_else(|| panic!("no example.com HTTP audit record in {records:#?}"));

    assert_eq!(
        record.get("outrig.action").and_then(Value::as_str),
        Some("allow")
    );
    assert_eq!(
        record.get("outrig.rule").and_then(Value::as_str),
        Some("default")
    );
    assert_eq!(record.get("proto").and_then(Value::as_str), Some("tcp"));
    assert_eq!(record.get("service").and_then(Value::as_str), Some("http"));
    assert_eq!(
        record.get("id.resp_h").and_then(Value::as_str),
        Some(host_ip.as_str()),
        "audit record should include the host destination IP",
    );
    assert!(
        record
            .get("orig_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        "audit record should include transmitted bytes: {record:#?}"
    );
    assert!(
        record
            .get("resp_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0,
        "audit record should include received bytes: {record:#?}"
    );
    assert!(
        record.get("duration").and_then(Value::as_f64).is_some(),
        "audit record should include duration: {record:#?}"
    );

    interceptor.shutdown().await;
    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filter_mode_denies_matching_host_before_upstream_bytes() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let mut container = Container::start(
        &image,
        ContainerLaunchSpec::workspace(workspace.path(), PathBuf::from("/workspace")),
    )
    .await
    .expect("start container");
    container.bootstrap_user().await.expect("bootstrap user");
    let host_ip = container_host_ipv4(&container);

    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Allow)
        .deny_host("example.com")
        .build()
        .expect("policy");
    let interceptor = NetworkInterceptor::start_with_policy(
        &container,
        &log_dir,
        container.session_suffix(),
        policy,
    )
    .await
    .expect("start network interceptor");

    let output = try_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args([
                "curl",
                "-fsS",
                "--connect-timeout",
                "5",
                "--max-time",
                "10",
                "--resolve",
            ])
            .arg(format!("example.com:443:{host_ip}"))
            .arg("https://example.com"),
    );
    assert!(
        !output.status.success(),
        "curl should fail when example.com is denied"
    );

    let records = read_audit_records(&log_dir).await;
    let record = records
        .iter()
        .find(|record| {
            record.get("outrig.host").and_then(Value::as_str) == Some("example.com")
                && record.get("id.resp_p").and_then(Value::as_u64) == Some(443)
        })
        .unwrap_or_else(|| panic!("no denied example.com:443 audit record in {records:#?}"));

    assert_eq!(
        record.get("outrig.action").and_then(Value::as_str),
        Some("deny")
    );
    assert_eq!(
        record.get("outrig.rule").and_then(Value::as_str),
        Some("deny[0]")
    );
    assert_eq!(
        record.get("id.resp_h").and_then(Value::as_str),
        Some(host_ip.as_str()),
        "audit record should include the curl --resolve destination IP",
    );
    assert_eq!(record.get("service").and_then(Value::as_str), Some("ssl"));
    assert_eq!(
        record.get("server_name").and_then(Value::as_str),
        Some("example.com")
    );
    assert_eq!(record.get("orig_bytes").and_then(Value::as_u64), Some(0));
    assert_eq!(record.get("resp_bytes").and_then(Value::as_u64), Some(0));

    interceptor.shutdown().await;
    container
        .stop(Duration::from_secs(2))
        .await
        .expect("stop container");
}
