//! A session's MCP services against real podman: the `mcp-fs` fixture placed as a command in the
//! primary, in a sidecar of its own, and as a primary view's entrypoint, through a
//! `SessionBuilder`; and a sidecar writing in a loop, paused and resumed.
//!
//! ```sh
//! cargo test -p outrig --features e2e --test session_mcp -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use outrig::SidecarSpec;
use outrig::config::{Config, SidecarWorkspaceAccess, Workspace};
use outrig::harness::{
    self, DEFAULT_DRAIN, McpPolicy, McpServiceOutcome, McpServiceSpec, McpStopHow, Quiesce,
    QuiesceOutcome, Session, SessionBuilder, Verdict,
};
use outrig::image::{self, ImageTag};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The fixture image `name`, built once and cached by its content.
async fn fixture_image(name: &str) -> ImageTag {
    image::ensure_image(&common::fixture_build_config(), &fixture(name), false)
        .await
        .unwrap_or_else(|e| panic!("ensure the {name} fixture: {e}"))
        .tag
}

/// The `mcp-fs` fixture with the filesystem server as its entrypoint, named directly: the view's
/// launcher runs an ELF program, not a script.
async fn entrypoint_image(fs: &ImageTag) -> ImageTag {
    let context = tempfile::tempdir().expect("a build context");
    std::fs::write(
        context.path().join("Dockerfile"),
        format!(
            "FROM {fs}\nENTRYPOINT [\"/usr/bin/node\", \
             \"/usr/local/lib/node_modules/@modelcontextprotocol/server-filesystem/dist/index.js\"]\n"
        ),
    )
    .expect("write the Dockerfile");
    image::ensure_image(&common::fixture_build_config(), context.path(), false)
        .await
        .expect("build the entrypoint image")
        .tag
}

/// A session with no primary agent in the `mcp-fs` image, its workspace `workspace`, logging
/// under `logs`, with `declare`'s MCP services.
async fn session(
    workspace: &Path,
    logs: &Path,
    declare: impl FnOnce(SessionBuilder) -> SessionBuilder,
) -> Session {
    let tag = fixture_image("mcp-fs").await;
    let mut config = Config::default();
    config.workspace = Workspace::new(workspace, "/workspace");
    config
        .images
        .insert("fs".to_string(), common::fixture_build_config());
    let spec = harness::container_spec(&config, "fs", Some(&tag), &fixture("mcp-fs"), logs.into())
        .await
        .expect("the session's container spec");
    let builder = SessionBuilder::new(config, None, None)
        .no_primary()
        .container(spec.spec);
    declare(builder)
        .start()
        .await
        .unwrap_or_else(|e| panic!("the session did not start: {e}"))
}

/// The filesystem server's argv, serving the workspace.
fn serve_workspace() -> Vec<String> {
    vec!["mcp-server-filesystem".into(), "/workspace".into()]
}

/// A command, a sidecar and a primary view each start with the filesystem server's tools, and
/// the close stops what each runs in.
#[tokio::test]
async fn three_placements_start_against_podman() {
    common::init_tracing();
    let fs = fixture_image("mcp-fs").await;
    let entry = entrypoint_image(&fs).await;
    let workspace = tempfile::tempdir().expect("a workspace");
    let logs = tempfile::tempdir().expect("a log dir");
    let session = session(workspace.path(), logs.path(), |builder| {
        builder
            .mcp("cmd", McpServiceSpec::command(serve_workspace()))
            .mcp(
                "side",
                McpServiceSpec::sidecar(
                    SidecarSpec::from_image("side", fs.as_str())
                        .with_workspace_access(SidecarWorkspaceAccess::Ro)
                        .with_server("fs", serve_workspace()),
                ),
            )
            .mcp(
                "view",
                McpServiceSpec::primary_view(entry.as_str()).with_args(["/workspace"]),
            )
    })
    .await;
    for (name, outcome) in session.mcp().outcomes() {
        let McpServiceOutcome::Ready { tools, .. } = outcome else {
            panic!("{name} did not start: {outcome:?}")
        };
        assert!(tools >= 3, "{name}: {tools} tools");
    }
    let report = session.shutdown(DEFAULT_DRAIN).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let hows: Vec<(String, McpStopHow, bool)> = report
        .mcp_services
        .iter()
        .map(|row| (row.service.clone(), row.how, row.proven))
        .collect();
    assert_eq!(
        hows,
        [
            ("cmd".to_string(), McpStopHow::WithPrimary, true),
            ("side".to_string(), McpStopHow::Stopped, true),
            ("view".to_string(), McpStopHow::Stopped, true),
        ]
    );
}

/// A sidecar whose server appends to a workspace file in a loop stops writing while it is
/// paused, and writes again once it is resumed.
#[tokio::test]
async fn a_paused_sidecar_stops_writing() {
    common::init_tracing();
    let fs = fixture_image("mcp-fs").await;
    let workspace = tempfile::tempdir().expect("a workspace");
    let logs = tempfile::tempdir().expect("a log dir");
    let ticking = vec![
        "sh".to_string(),
        "-c".to_string(),
        "(while true; do echo tick >> /workspace/ticks; sleep 0.1; done) & \
         exec mcp-server-filesystem /workspace"
            .to_string(),
    ];
    let session = session(workspace.path(), logs.path(), |builder| {
        builder.mcp(
            "writer",
            McpServiceSpec::sidecar(
                SidecarSpec::from_image("writer", fs.as_str())
                    .with_workspace_access(SidecarWorkspaceAccess::Rw)
                    .with_server("fs", ticking),
            ),
        )
    })
    .await;
    assert!(
        matches!(
            session.mcp().outcomes()[0].1,
            McpServiceOutcome::Ready { .. }
        ),
        "{:?}",
        session.mcp().outcomes()
    );
    let ticks = workspace.path().join("ticks");
    let size = || std::fs::metadata(&ticks).map_or(0, |meta| meta.len());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(size() > 0, "the writer writes");

    let paused = session
        .mcp()
        .quiesce("writer", Quiesce::Pause, Duration::from_secs(5))
        .await;
    assert!(
        matches!(paused, QuiesceOutcome::Quiesced { .. }),
        "{paused:?}"
    );
    let frozen = size();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(size(), frozen, "nothing was written while paused");

    session
        .mcp()
        .resume("writer")
        .await
        .expect("the writer resumes");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(size() > frozen, "the writer writes again");
    let report = session.shutdown(DEFAULT_DRAIN).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
}

/// An entrypoint sidecar whose server never answers is cut off at the connect budget, the
/// container it made still running, and the close stops that container and proves it: nothing
/// of the session's is left.
#[tokio::test]
async fn an_attempt_cut_off_at_the_budget_leaves_no_container() {
    common::init_tracing();
    let fs = fixture_image("mcp-fs").await;
    let context = tempfile::tempdir().expect("a build context");
    std::fs::write(
        context.path().join("Dockerfile"),
        format!("FROM {fs}\nENTRYPOINT [\"sleep\", \"infinity\"]\n"),
    )
    .expect("write the Dockerfile");
    let silent = image::ensure_image(&common::fixture_build_config(), context.path(), false)
        .await
        .expect("build the silent image")
        .tag;
    let workspace = tempfile::tempdir().expect("a workspace");
    let logs = tempfile::tempdir().expect("a log dir");
    let session = session(workspace.path(), logs.path(), |builder| {
        builder
            .mcp(
                "silent",
                McpServiceSpec::sidecar(
                    SidecarSpec::from_image("silent", silent.as_str())
                        .with_entrypoint_server("silent", Vec::<String>::new()),
                ),
            )
            .mcp_policy(McpPolicy::default().with_connect_budget(Duration::from_secs(3)))
    })
    .await;
    let McpServiceOutcome::Unavailable(cause) = &session.mcp().outcomes()[0].1 else {
        panic!("{:?}", session.mcp().outcomes())
    };
    // Cut off by the budget, which began before the container was made -- not by the
    // handshake's own bound, which began after.
    assert!(cause.message.contains("had not finished"), "{cause:?}");
    let id = session.id().to_string();
    let report = session.shutdown(DEFAULT_DRAIN).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let row = &report.mcp_services[0];
    assert_eq!((row.how, row.proven), (McpStopHow::NotStarted, true));
    let listed = std::process::Command::new("podman")
        .args(["ps", "-a", "--format", "{{.Names}}"])
        .output()
        .expect("podman ps");
    let left: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter(|name| name.contains(&id))
        .map(str::to_string)
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}
