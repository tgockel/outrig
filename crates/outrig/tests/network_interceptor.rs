//! End-to-end smoke for audit-mode network interception. Gated behind
//! `--features e2e` because it needs podman/buildah and nftables namespace
//! access. [`a_resolved_name_grants_a_hostname_allow`] additionally needs
//! working outbound DNS, since resolving through the interceptor is the whole
//! point of it.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e --test network_interceptor -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

use outrig::config::{NetworkAction, NetworkPolicy};
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
    let cfg = common::fixture_build_config();
    image::ensure_image(&cfg, context, false)
        .await
        .expect("ensure curl image")
        .tag
}

async fn read_audit_records(log_dir: &Path) -> Vec<Value> {
    read_audit_records_until(log_dir, |records| !records.is_empty()).await
}

/// Polls the audit log until `ready` accepts the parsed records, so tests
/// that expect records from multiple connections do not race their flush.
async fn read_audit_records_until(log_dir: &Path, ready: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let path = log_dir.join("network.jsonl");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| serde_json::from_str(line).expect("valid audit JSON"))
                    .collect()
            })
            .unwrap_or_default();
        if ready(&records) {
            return records;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for records in {}",
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

fn container_root_pid(container: &Container) -> u32 {
    let output = run_capture(Command::new("podman").args([
        "inspect",
        "--format",
        "{{.State.Pid}}",
        container.name(),
    ]));
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .expect("container pid")
}

fn netns_has_outrig_table(pid: u32) -> bool {
    let output = run_capture(Command::new("nsenter").args([
        "-t",
        &pid.to_string(),
        "-U",
        "-n",
        "nft",
        "list",
        "tables",
    ]));
    String::from_utf8_lossy(&output.stdout).contains("outrig_")
}

fn record_host(record: &Value) -> Option<&str> {
    record.get("outrig.host").and_then(Value::as_str)
}

fn record_container(record: &Value) -> Option<&str> {
    record.get("outrig.container").and_then(Value::as_str)
}

async fn start_bootstrapped_container(image: &ImageTag, workspace: &Path) -> Container {
    let mut container = Container::start(
        image,
        ContainerLaunchSpec::workspace(workspace, PathBuf::from("/workspace")),
    )
    .await
    .expect("start container");
    container.bootstrap_user().await.expect("bootstrap user");
    container
}

fn curl_cmd(container: &Container, extra_args: &[&str], url: &str) -> Command {
    let mut cmd = Command::new("podman");
    cmd.arg("exec")
        .arg(container.name())
        .args(["curl", "-fsS", "--connect-timeout", "5", "--max-time", "10"])
        .args(extra_args)
        .arg(url);
    cmd
}

/// Shuts the interceptor down, asserts no outrig nft table survives in either
/// container's netns, and stops the containers.
async fn shutdown_and_stop(interceptor: NetworkInterceptor, containers: [Container; 2]) {
    let pids = containers.each_ref().map(container_root_pid);
    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
    for pid in pids {
        assert!(
            !netns_has_outrig_table(pid),
            "shutdown should delete every attached container's nft table"
        );
    }
    for container in containers {
        container
            .stop(Duration::from_secs(2))
            .await
            .expect("stop container");
    }
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

/// A fixture that accepts one connection and then holds it open, silent, so a
/// request made through the interceptor stays live -- and unrecorded -- until
/// something cuts it. The channel reports the accept, so a test can be sure it
/// is cutting a connection that exists.
fn start_silent_fixture() -> (
    SocketAddr,
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("bind silent fixture");
    let addr = listener.local_addr().expect("silent fixture addr");
    let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = accepted_tx.send(());
            // Never answers. The connection ends when the interceptor cuts it
            // or when this fixture gives up, whichever comes first.
            std::thread::sleep(Duration::from_secs(15));
            drop(stream);
        }
    });
    (addr, accepted_rx, handle)
}

fn read_resolv_conf(container: &Container) -> Vec<u8> {
    run_capture(
        Command::new("podman")
            .arg("exec")
            .arg(container.name())
            .args(["cat", "/etc/resolv.conf"]),
    )
    .stdout
}

/// The inverse-pair property against a real container: what `attach` changed,
/// `detach` changes back, and the connections `attach`'s listeners accepted
/// are cut *and recorded* by the time `detach` returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_restores_the_resolver_and_cuts_what_it_accepted() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let dir = tempfile::tempdir().expect("tempdir");
    let context = dir.path().join("image");
    std::fs::create_dir_all(&context).expect("image context dir");
    write_curl_image_context(&context);
    let image = ensure_curl_image(&context).await;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let log_dir = dir.path().join("logs");

    let container = start_bootstrapped_container(&image, &workspace).await;
    let before = read_resolv_conf(&container);

    let mut interceptor =
        NetworkInterceptor::start(&container, &log_dir, container.session_suffix())
            .await
            .expect("start interceptor");

    let during = read_resolv_conf(&container);
    assert_ne!(
        during, before,
        "attach should have installed its own resolver"
    );
    assert!(
        String::from_utf8_lossy(&during).contains("nameserver 127.0.0.1"),
        "resolver under interception: {}",
        String::from_utf8_lossy(&during)
    );

    // A request nothing will ever answer, so the connection is still open --
    // and still unrecorded -- when the detach lands on it.
    let (fixture, accepted, fixture_thread) = start_silent_fixture();
    let host = container_host_ipv4(&container);
    let mut held = Command::new("podman")
        .arg("exec")
        .arg(container.name())
        .args(["curl", "-sS", "--max-time", "60"])
        .arg(format!("http://{host}:{}/held", fixture.port()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn the held request");
    accepted
        .recv_timeout(Duration::from_secs(15))
        .expect("the container's request should reach the fixture");

    interceptor
        .detach(container.name())
        .await
        .expect("detach the container");

    // Read the instant `detach` returned, with no polling: the record for a
    // connection detach cut is an obligation it discharges before returning,
    // not one it leaves in flight behind it.
    let log = std::fs::read_to_string(log_dir.join("network.jsonl")).expect("read audit log");
    assert!(
        log.lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).expect("valid audit JSON"))
            .any(|record| record["id.resp_p"] == fixture.port()),
        "the cut connection should already be recorded, log was: {log}"
    );

    let status = held.wait().expect("wait for the held request");
    assert!(
        !status.success(),
        "the held request should have been cut, not completed"
    );

    // "Recorded by the time detach returned" and "nothing is written after
    // it" are different claims, and only the first is checked above. A
    // connection that was merely unwatched rather than ended would still be
    // holding a record it had not written yet.
    let settled = log.lines().filter(|line| !line.trim().is_empty()).count();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let after = std::fs::read_to_string(log_dir.join("network.jsonl")).expect("re-read audit log");
    assert_eq!(
        after.lines().filter(|line| !line.trim().is_empty()).count(),
        settled,
        "no further records may be written after detach returned, log was: {after}"
    );

    let after = read_resolv_conf(&container);
    assert_eq!(
        after, before,
        "detach should restore the resolver byte for byte"
    );

    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
    container
        .stop(Duration::from_secs(2))
        .await
        .expect("stop container");
    let _ = fixture_thread.join();
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

    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
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

    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
    container
        .stop(Duration::from_secs(2))
        .await
        .expect("stop container");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_containers_attribute_audit_records_and_detach_cleanly() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let container_a = start_bootstrapped_container(&image, workspace_a.path()).await;
    let host_ip_a = container_host_ipv4(&container_a);

    let mut interceptor =
        NetworkInterceptor::start(&container_a, &log_dir, container_a.session_suffix())
            .await
            .expect("start network interceptor");

    let (addr_a, fixture_a) = start_http_fixture();
    run_capture(&mut curl_cmd(
        &container_a,
        &["-H", "Host: example-a.com"],
        &format!("http://{host_ip_a}:{}/", addr_a.port()),
    ));
    let _ = fixture_a.join();

    // Attach the second container mid-session, after traffic already flowed
    // through the first -- the shape dynamic sidecar addition relies on.
    let container_b = start_bootstrapped_container(&image, workspace_b.path()).await;
    let host_ip_b = container_host_ipv4(&container_b);
    interceptor
        .attach(&container_b)
        .await
        .expect("attach container b");

    let (addr_b, fixture_b) = start_http_fixture();
    run_capture(&mut curl_cmd(
        &container_b,
        &["-H", "Host: example-b.com"],
        &format!("http://{host_ip_b}:{}/", addr_b.port()),
    ));
    let _ = fixture_b.join();

    let records = read_audit_records_until(&log_dir, |records| {
        ["example-a.com", "example-b.com"]
            .iter()
            .all(|host| records.iter().any(|r| record_host(r) == Some(host)))
    })
    .await;
    for (host, container) in [
        ("example-a.com", container_a.name()),
        ("example-b.com", container_b.name()),
    ] {
        let record = records
            .iter()
            .find(|r| record_host(r) == Some(host))
            .unwrap_or_else(|| panic!("no {host} audit record in {records:#?}"));
        assert_eq!(
            record_container(record),
            Some(container),
            "record for {host} should be attributed to {container}: {record:#?}"
        );
    }

    let pid_a = container_root_pid(&container_a);
    let pid_b = container_root_pid(&container_b);
    assert!(netns_has_outrig_table(pid_a));
    assert!(netns_has_outrig_table(pid_b));

    interceptor
        .detach(container_b.name())
        .await
        .expect("detach container b");
    assert!(
        !netns_has_outrig_table(pid_b),
        "detach should delete container b's nft table"
    );
    assert!(
        netns_has_outrig_table(pid_a),
        "detach of b must not disturb container a's nft table"
    );

    shutdown_and_stop(interceptor, [container_a, container_b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_containers_filter_mode_attributes_denials() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let container_a = start_bootstrapped_container(&image, workspace_a.path()).await;
    let container_b = start_bootstrapped_container(&image, workspace_b.path()).await;
    let host_ip_a = container_host_ipv4(&container_a);
    let host_ip_b = container_host_ipv4(&container_b);

    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Allow)
        .deny_host("example.com")
        .build()
        .expect("policy");
    let mut interceptor = NetworkInterceptor::start_with_policy(
        &container_a,
        &log_dir,
        container_a.session_suffix(),
        policy,
    )
    .await
    .expect("start network interceptor");
    interceptor
        .attach(&container_b)
        .await
        .expect("attach container b");

    for (container, host_ip) in [(&container_a, &host_ip_a), (&container_b, &host_ip_b)] {
        let output = try_capture(&mut curl_cmd(
            container,
            &["--resolve", &format!("example.com:443:{host_ip}")],
            "https://example.com",
        ));
        assert!(
            !output.status.success(),
            "curl from {} should fail when example.com is denied",
            container.name()
        );
    }

    let records = read_audit_records_until(&log_dir, |records| {
        [container_a.name(), container_b.name()]
            .iter()
            .all(|container| {
                records
                    .iter()
                    .any(|r| record_container(r) == Some(container))
            })
    })
    .await;
    for container in [container_a.name(), container_b.name()] {
        let record = records
            .iter()
            .find(|r| record_container(r) == Some(container))
            .unwrap_or_else(|| panic!("no audit record for {container} in {records:#?}"));
        assert_eq!(record_host(record), Some("example.com"));
        assert_eq!(
            record.get("outrig.action").and_then(Value::as_str),
            Some("deny")
        );
        assert_eq!(
            record.get("outrig.rule").and_then(Value::as_str),
            Some("deny[0]")
        );
        assert_eq!(record.get("orig_bytes").and_then(Value::as_u64), Some(0));
        assert_eq!(record.get("resp_bytes").and_then(Value::as_u64), Some(0));
    }

    shutdown_and_stop(interceptor, [container_a, container_b]).await;
}

/// A hostname rule grants against a destination the container resolved through
/// the interceptor's own DNS listener. This is the test that fails if the fix
/// for the forged-identity bypass stops at refusing client-asserted names: with
/// nothing else granting, `allow[0]` can only have come from a validated
/// binding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resolved_name_grants_a_hostname_allow() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let container = start_bootstrapped_container(&image, workspace.path()).await;

    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Deny)
        .allow_host_port("example.com", 443)
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

    // No `--resolve`: the name has to go through the interceptor's DNS
    // listener, which is what earns the binding.
    let _ = try_capture(&mut curl_cmd(&container, &[], "https://example.com/"));

    let records = read_audit_records_until(&log_dir, |records| {
        records
            .iter()
            .any(|record| record.get("id.resp_p").and_then(Value::as_u64) == Some(443))
    })
    .await;
    let record = records
        .iter()
        .find(|record| record.get("id.resp_p").and_then(Value::as_u64) == Some(443))
        .unwrap_or_else(|| panic!("no example.com:443 audit record in {records:#?}"));

    assert_eq!(
        record.get("outrig.action").and_then(Value::as_str),
        Some("allow"),
        "a resolved name should grant its hostname rule: {record:#?}"
    );
    assert_eq!(
        record.get("outrig.rule").and_then(Value::as_str),
        Some("allow[0]")
    );

    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
    container
        .stop(Duration::from_secs(2))
        .await
        .expect("stop container");
}

/// The audited bypass, live: connect straight to an address and claim the
/// allowlisted name in the `ClientHello`. The claim never reaches the allow
/// list, and the record says the name in it was asserted rather than resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_sni_does_not_grant_a_hostname_allow() {
    let _guard = E2E_LOCK.lock().await;
    common::init_tracing();

    let image_context = tempfile::tempdir().expect("image context");
    write_curl_image_context(image_context.path());
    let image = ensure_curl_image(image_context.path()).await;

    let workspace = tempfile::tempdir().expect("workspace");
    let session = tempfile::tempdir().expect("session");
    let log_dir = session.path().join("logs");

    let container = start_bootstrapped_container(&image, workspace.path()).await;
    let host_ip = container_host_ipv4(&container);

    let policy = NetworkPolicy::builder()
        .default_action(NetworkAction::Deny)
        .allow_host_port("allowed.test", 443)
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

    let output = try_capture(&mut curl_cmd(
        &container,
        &["--resolve", &format!("allowed.test:443:{host_ip}")],
        "https://allowed.test/",
    ));
    assert!(
        !output.status.success(),
        "a client-asserted name must not grant a hostname allow"
    );

    let records = read_audit_records(&log_dir).await;
    let record = records
        .iter()
        .find(|record| record.get("id.resp_p").and_then(Value::as_u64) == Some(443))
        .unwrap_or_else(|| panic!("no allowed.test:443 audit record in {records:#?}"));

    assert_eq!(
        record.get("outrig.action").and_then(Value::as_str),
        Some("deny")
    );
    assert_eq!(
        record.get("outrig.rule").and_then(Value::as_str),
        Some("default"),
        "the allow entry must not have matched: {record:#?}"
    );
    assert_eq!(record_host(record), Some("allowed.test"));
    assert_eq!(
        record.get("outrig.host_source").and_then(Value::as_str),
        Some("asserted"),
        "the record should say the name in it was a claim: {record:#?}"
    );
    assert_eq!(record.get("orig_bytes").and_then(Value::as_u64), Some(0));

    interceptor
        .shutdown()
        .await
        .expect("shutdown the interceptor");
    container
        .stop(Duration::from_secs(2))
        .await
        .expect("stop container");
}
