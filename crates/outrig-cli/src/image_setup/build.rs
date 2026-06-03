//! `outrig image build` -- build a standalone image project and validate it.
//!
//! Reads `PROJECT_DIR/image.toml`, serializes it into OCI labels, builds the
//! declared image with buildah (tagged by `[image].ref`, or `--tag <ref>`, and
//! stamped with those labels), then starts a throwaway container to prove the
//! result is a usable OutRig toolset image: it must carry a valid
//! `org.outrig.mcp` label, and -- unless `--no-test` -- every declared MCP
//! server must initialize and answer `tools/list`.
//!
//! Unlike repo-local `outrig build`, there is no content-addressed cache: the
//! output is a caller-named ref, so `--no-cache` only forwards to buildah.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use outrig::McpClient;
use outrig::container::embedded::{
    StandaloneImageToml, parse_standalone_image_toml, read_standalone_image_mcp,
    standalone_config_to_labels,
};
use outrig::container::{Container, ContainerLaunchSpec};
use outrig::image::{self, ImageTag};

use crate::cli::session_setup::plural;
use crate::error::{OutrigError, Result};

const STOP_GRACE: Duration = Duration::from_secs(2);

/// Build the standalone image project in `dir` (relative to `cwd`, or `cwd`
/// itself when `dir` is `None`), then validate the built image.
pub async fn run(
    cwd: &Path,
    dir: Option<&Path>,
    tag_override: Option<&str>,
    no_test: bool,
    no_cache: bool,
) -> Result<()> {
    let project_dir = dir.map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
    let parsed = load_project_image_toml(&project_dir)?;
    let labels = standalone_config_to_labels(&parsed)?;
    let tag = ImageTag(
        tag_override
            .map(str::to_string)
            .unwrap_or_else(|| parsed.image.image_ref.clone()),
    );

    eprintln!("[outrig] building image {tag}");
    eprintln!(
        "[outrig]   dockerfile: {}",
        parsed.build.dockerfile.display()
    );
    eprintln!("[outrig]   context:    {}", parsed.build.context.display());
    image::build_standalone(
        &project_dir,
        &parsed.build.dockerfile,
        &parsed.build.context,
        &tag,
        no_cache,
        &labels,
    )
    .await?;
    eprintln!("[outrig] image ready");

    // One throwaway tempdir holds both the workspace bind-mount target and the
    // per-server MCP stderr logs; it (and the logs) vanish when `run` returns.
    // Startup/tools-list failures are already enriched into the returned error,
    // so discarding the happy-path logs is fine for a validation command.
    let scratch = tempfile::tempdir()?;
    let host_ws = scratch.path().join("workspace");
    std::fs::create_dir_all(&host_ws)?;
    let log_dir = scratch.path().join("logs");

    let mut container = Container::start(
        &tag,
        ContainerLaunchSpec::workspace(&host_ws, Path::new("/workspace")),
    )
    .await?;

    // Validate (and optionally test) against the container, then tear down on
    // both paths -- the validation error wins over a stop error. `Drop` is the
    // backstop for the window between `start` and `stop`.
    let outcome = validate_and_test(&mut container, &log_dir, no_test).await;
    let stop = container.stop(STOP_GRACE).await;
    outcome?;
    stop?;

    eprintln!("[outrig] image ok");
    Ok(())
}

/// Read and parse the project's own `image.toml`. Failures (missing file,
/// malformed TOML, missing required fields) are framed against the project path
/// -- this is the user's input, distinct from the stamped labels validated
/// later via [`read_standalone_image_mcp`].
fn load_project_image_toml(project_dir: &Path) -> Result<StandaloneImageToml> {
    let path = project_dir.join("image.toml");
    let text = std::fs::read_to_string(&path).map_err(|source| {
        OutrigError::Configuration(format!("could not read {}: {source}", path.display()))
    })?;
    let parsed = parse_standalone_image_toml(&text)
        .map_err(|source| OutrigError::Configuration(format!("{}: {source}", path.display())))?;
    Ok(parsed)
}

/// Read+validate the stamped `org.outrig.mcp` label (always), then -- unless
/// `no_test` -- start each declared MCP server and report its tool count.
async fn validate_and_test(container: &mut Container, log_dir: &Path, no_test: bool) -> Result<()> {
    let mcp = read_standalone_image_mcp(container).await?;
    let count = mcp.len();
    eprintln!(
        "[outrig] image config validated ({count} mcp {})",
        plural(count, "server", "servers")
    );

    if no_test {
        eprintln!("[outrig] skipping live mcp test (--no-test)");
        return Ok(());
    }

    container.bootstrap_user().await?;
    for (name, spec) in &mcp {
        let client =
            McpClient::connect_via_podman_exec(container, spec, name, log_dir, &BTreeMap::new())
                .await?;
        let tools = client.list_tools().await?.len();
        eprintln!(
            "[outrig] mcp {name}: initialized ({tools} {})",
            plural(tools, "tool", "tools")
        );
        client.shutdown().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_errors_when_image_toml_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = load_project_image_toml(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("could not read"), "{err}");
        assert!(err.to_string().contains("image.toml"), "{err}");
    }

    #[test]
    fn load_errors_on_malformed_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("image.toml"), "[image\nref =").unwrap();
        let err = load_project_image_toml(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("image.toml"), "{err}");
    }

    #[test]
    fn load_errors_on_missing_image_ref() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("image.toml"), "[mcp]\nfs = [\"x\"]\n").unwrap();
        let err = load_project_image_toml(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("image.ref is required"), "{err}");
    }
}
