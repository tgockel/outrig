//! Integration tests for `Config::validate`, `merge`, and `Config::load`.
//! Mirrors every rule in `doc/reference/config.md`'s "Validation rules" plus
//! the merge semantics documented in "Resolution: which file wins".

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use outrig::config::{
    Config, ConfigSource, ConfigValidationError, ImageConfig, LlmProvider, McpServerSpec,
    MountAccess, MountConfig, MountRuleViolation, NetworkAction, NetworkEntry, NetworkMode,
    SidecarOnFailure, SidecarStart, SidecarView, SidecarWorkspaceAccess, merge,
};
use outrig::error::OutrigError;

const FIXTURE_FULL: &str = include_str!("fixtures/config-full.toml");

fn parse(s: &str) -> Config {
    Config::load_from_str(s).expect("config parses")
}

fn expect_validation_err(cfg: &Config, repo_root: Option<&Path>) -> ConfigValidationError {
    match cfg.validate(repo_root) {
        Err(OutrigError::ConfigValidation(e)) => e,
        Err(other) => panic!("expected ConfigValidation, got: {other:?}"),
        Ok(()) => panic!("expected validation error, got Ok"),
    }
}

/// Unwrap the validation error out of a failed `Config::load`.
fn expect_load_validation_err(err: OutrigError) -> ConfigValidationError {
    match err {
        OutrigError::ConfigValidation(e) => e,
        other => panic!("expected ConfigValidation, got: {other:?}"),
    }
}

/// Write `body` to `<root>/.agents/outrig/config.toml`, creating the tree.
fn write_repo_cfg(root: &Path, body: &str) {
    let agents = root.join(".agents/outrig");
    fs::create_dir_all(&agents).unwrap();
    fs::write(agents.join("config.toml"), body).unwrap();
}

/// Write `body` to `<dir>/config.toml` and return the path, for use as a
/// `--global-config` target.
fn write_global_cfg(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    fs::write(&path, body).unwrap();
    path
}

mod config_validate {
    use super::*;

    #[test]
    fn all_good_validates_clean() {
        let cfg = parse(FIXTURE_FULL);
        cfg.validate(None).expect("fixture validates structurally");
    }

    #[test]
    fn network_filter_policy_validates_clean() {
        let cfg = parse(
            r#"
[network]
mode    = "filter"
default = "allow"
allow   = ["github.com:443", "*.npmjs.org", "10.0.0.0/8", "[2001:db8::1]:443"]
deny    = ["*:22", { host = "169.254.169.254", port = 80 }]
"#,
        );
        cfg.validate(None).expect("network filter policy validates");
        assert_eq!(cfg.network.mode, NetworkMode::Filter);
        assert_eq!(cfg.network.default, NetworkAction::Allow);
        assert_eq!(
            cfg.network.allow[0],
            NetworkEntry::with_port("github.com", 443),
        );
        assert_eq!(cfg.network.allow[1], NetworkEntry::new("*.npmjs.org"));
        assert_eq!(cfg.network.allow[2], NetworkEntry::new("10.0.0.0/8"));
        assert_eq!(
            cfg.network.allow[3],
            NetworkEntry::with_port("2001:db8::1", 443),
        );
        assert_eq!(cfg.network.deny[0], NetworkEntry::with_port("*", 22),);
        assert_eq!(
            cfg.network.deny[1],
            NetworkEntry::with_port("169.254.169.254", 80),
        );
    }

    #[test]
    fn network_filter_requires_at_least_one_entry() {
        let cfg = parse(
            r#"
[network]
mode = "filter"
default = "allow"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::NetworkPolicyInvalid { ref message } if message.contains("requires at least one")),
            "got: {err:?}",
        );
    }

    #[test]
    fn network_policy_rejects_malformed_host_and_zero_port() {
        let malformed = parse(
            r#"
[network]
mode  = "filter"
allow = ["bad host"]
"#,
        );
        let err = expect_validation_err(&malformed, None);
        assert!(
            matches!(err, ConfigValidationError::NetworkPolicyInvalid { ref message } if message.contains("whitespace")),
            "got: {err:?}",
        );

        let zero_port = parse(
            r#"
[network]
mode  = "filter"
allow = [{ host = "example.com", port = 0 }]
"#,
        );
        let err = expect_validation_err(&zero_port, None);
        assert!(
            matches!(err, ConfigValidationError::NetworkPolicyInvalid { ref message } if message.contains("between 1 and 65535")),
            "got: {err:?}",
        );
    }

    #[test]
    fn dangling_default_image_errors() {
        let cfg = parse(
            r#"
default-image = "missing"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::UnknownDefaultImage { ref name } if name == "missing"),
            "got: {err:?}",
        );
    }

    #[test]
    fn dangling_default_agent_errors() {
        let cfg = parse(
            r#"
default-agent = "ghost"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::UnknownDefaultAgent { ref name } if name == "ghost"),
            "got: {err:?}",
        );
    }

    /// `outrig` is reserved so the built-in `outrig__<tool>` names cannot be
    /// shadowed by a configured server's tools.
    #[test]
    fn mcp_server_named_outrig_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.mcp]
outrig = ["some-server"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::ReservedMcpServerName { ref server, .. }
                    if server == "outrig"
            ),
            "got: {err:?}",
        );
    }

    #[test]
    fn dangling_default_model_errors() {
        let cfg = parse(
            r#"
default-model = "phantom"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::UnknownDefaultModel { ref name } if name == "phantom"),
            "got: {err:?}",
        );
    }

    #[test]
    fn dangling_agent_model_errors() {
        let cfg = parse(
            r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.review]
model = "missing"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::UnknownAgentModel { agent, model } => {
                assert_eq!(agent, "review");
                assert_eq!(model, "missing");
            }
            other => panic!("expected UnknownAgentModel, got: {other:?}"),
        }
    }

    #[test]
    fn dangling_model_provider_errors() {
        let cfg = parse(
            r#"
[models.fast]
provider   = "ghost"
identifier = "gpt-4o-mini"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::UnknownModelProvider { model, provider } => {
                assert_eq!(model, "fast");
                assert_eq!(provider, "ghost");
            }
            other => panic!("expected UnknownModelProvider, got: {other:?}"),
        }
    }

    #[test]
    fn dangling_agent_container_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
image = "ghost"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::UnknownAgentImage { agent, image } => {
                assert_eq!(agent, "coding");
                assert_eq!(image, "ghost");
            }
            other => panic!("expected UnknownAgentImage, got: {other:?}"),
        }
    }

    #[test]
    fn agent_omits_model_and_no_default_errors() {
        let cfg = parse(
            r#"
[agents.coding]
preamble = "hi"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::AgentMissingModel { agent } => {
                assert_eq!(agent, "coding");
            }
            other => panic!("expected AgentMissingModel, got: {other:?}"),
        }
    }

    #[test]
    fn agent_omits_model_with_default_resolves() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
preamble = "hi"
"#,
        );
        cfg.validate(None)
            .expect("agent without model resolves through default-model");
    }

    #[test]
    fn mcp_server_name_invalid_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

  [images.coding.mcp]
  "bad name" = ["bin"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::InvalidMcpServerName { image, server } => {
                assert_eq!(image, "coding");
                assert_eq!(server, "bad name");
            }
            other => panic!("expected InvalidMcpServerName, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_server_name_leading_dash_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

  [images.coding.mcp]
  "-leading" = ["bin"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(matches!(
            err,
            ConfigValidationError::InvalidMcpServerName { .. }
        ));
    }

    #[test]
    fn build_image_name_invalid_errors() {
        // A build image's name becomes its container image repository, so an
        // uppercase / spaced name is rejected at config load.
        let cfg = parse(
            r#"
[images."Bad Name"]
dockerfile = "D"
context    = "ctx"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::BuildImageNameInvalid { image } => {
                assert_eq!(image, "Bad Name");
            }
            other => panic!("expected BuildImageNameInvalid, got: {other:?}"),
        }
    }

    #[test]
    fn build_image_repo_specific_name_validates() {
        let cfg = parse(
            r#"
[images.outrig-standard]
dockerfile = "D"
context    = "ctx"
"#,
        );
        cfg.validate(None)
            .expect("repo-specific lowercase build image name validates");
    }

    #[test]
    fn image_name_config_skips_build_name_check() {
        // Image-name (pull) configs use `image-name` as the tag, so the block
        // key is just a label and isn't constrained to a repository grammar.
        let cfg = parse(
            r#"
[images."Pull Only"]
image-name = "docker.io/library/alpine:latest"
"#,
        );
        cfg.validate(None)
            .expect("image-name config block key is not constrained");
    }

    #[test]
    fn mcp_command_empty_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

  [images.coding.mcp]
  srv = []
"#,
        );
        // Sanity: shape is Short with empty command.
        assert!(matches!(
            cfg.images["coding"].mcp["srv"],
            McpServerSpec::Short(ref v) if v.is_empty(),
        ));
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::EmptyMcpCommand { image, server } => {
                assert_eq!(image, "coding");
                assert_eq!(server, "srv");
            }
            other => panic!("expected EmptyMcpCommand, got: {other:?}"),
        }
    }

    #[test]
    fn session_root_relative_errors() {
        let cfg = parse(
            r#"
session-root = "relative/path"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SessionRootNotAbsolute { path } => {
                assert_eq!(path, std::path::PathBuf::from("relative/path"));
            }
            other => panic!("expected SessionRootNotAbsolute, got: {other:?}"),
        }
    }

    #[test]
    fn dockerfile_missing_on_disk_errors() {
        let tmp = tempdir().unwrap();
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "Dockerfile"
context    = "."
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        let err_text = err.to_string();
        match err {
            ConfigValidationError::DockerfileMissing {
                image,
                path,
                declared_in,
                ..
            } => {
                assert_eq!(image, "coding");
                assert_eq!(path, std::path::PathBuf::from("Dockerfile"));
                // Parsed with `load_from_str`, so nothing recorded a source.
                // The diagnostic names no file rather than guessing one --
                // there is no config file here that mentions this image.
                assert_eq!(
                    declared_in, None,
                    "a sourceless entry must not invent a declaring file",
                );
                assert!(
                    !err_text.contains("declared in"),
                    "the clause is omitted entirely, not rendered as None: {err_text}",
                );
            }
            other => panic!("expected DockerfileMissing, got: {other:?}"),
        }
    }

    #[test]
    fn context_missing_on_disk_errors() {
        let tmp = tempdir().unwrap();
        // dockerfile exists but context doesn't.
        fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "Dockerfile"
context    = "missing-ctx"
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        match err {
            ConfigValidationError::ContextMissing {
                image,
                path,
                declared_in,
                ..
            } => {
                assert_eq!(image, "coding");
                assert_eq!(path, std::path::PathBuf::from("missing-ctx"));
                assert_eq!(declared_in, None);
            }
            other => panic!("expected ContextMissing, got: {other:?}"),
        }
    }

    #[test]
    fn disk_checks_skipped_when_repo_root_is_none() {
        // Same shape as dockerfile_missing_on_disk_errors -- but with
        // `repo_root = None`, the on-disk existence check is skipped.
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "does/not/exist/Dockerfile"
context    = "does/not/exist"
"#,
        );
        cfg.validate(None)
            .expect("structural-only validate ignores disk paths");
    }

    #[test]
    fn workspace_mount_missing_host_errors() {
        let tmp = tempdir().unwrap();
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "missing-docs"
container-path = "/resources/docs"
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        match err {
            ConfigValidationError::WorkspaceMountHostMissing { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("missing-docs"));
            }
            other => panic!("expected WorkspaceMountHostMissing, got: {other:?}"),
        }
    }

    #[test]
    fn workspace_mount_file_host_errors() {
        let tmp = tempdir().unwrap();
        fs::write(tmp.path().join("docs.txt"), "not a directory").unwrap();
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "docs.txt"
container-path = "/resources/docs"
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        match err {
            ConfigValidationError::WorkspaceMountHostNotDirectory { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("docs.txt"));
            }
            other => panic!("expected WorkspaceMountHostNotDirectory, got: {other:?}"),
        }
    }

    #[test]
    fn workspace_mount_container_path_must_be_absolute() {
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "docs"
container-path = "resources/docs"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::WorkspaceMountContainerNotAbsolute { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("resources/docs"));
            }
            other => panic!("expected WorkspaceMountContainerNotAbsolute, got: {other:?}"),
        }
    }

    #[test]
    fn workspace_mount_container_path_must_not_be_root() {
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "docs"
container-path = "/"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::WorkspaceMountContainerRoot { .. }
            ),
            "expected WorkspaceMountContainerRoot, got: {err:?}",
        );
    }

    #[test]
    fn workspace_mount_duplicate_container_path_errors() {
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "docs-a"
container-path = "/resources/docs"

[[workspace.mounts]]
host-path      = "docs-b"
container-path = "/resources/docs"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::WorkspaceMountContainerDuplicate { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("/resources/docs"));
            }
            other => panic!("expected WorkspaceMountContainerDuplicate, got: {other:?}"),
        }
    }

    #[test]
    fn workspace_mount_cannot_shadow_primary_workspace() {
        let cfg = parse(
            r#"
[workspace]
host-path      = "."
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "docs"
container-path = "/workspace"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::WorkspaceMountContainerDuplicate { path, .. } => {
                assert_eq!(path, std::path::PathBuf::from("/workspace"));
            }
            other => panic!("expected WorkspaceMountContainerDuplicate, got: {other:?}"),
        }
    }

    #[test]
    fn malformed_capability_name_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
cap-drop = ["net_raw"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::CapabilityNameInvalid {
                image,
                field,
                capability,
            } => {
                assert_eq!(image, "coding");
                assert_eq!(field, "cap-drop");
                assert_eq!(capability, "net_raw");
            }
            other => panic!("expected CapabilityNameInvalid, got: {other:?}"),
        }
    }

    #[test]
    fn empty_capability_name_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
cap-add = ["CAP_"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::CapabilityNameEmpty { image, field } => {
                assert_eq!(image, "coding");
                assert_eq!(field, "cap-add");
            }
            other => panic!("expected CapabilityNameEmpty, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_capability_names_error_after_prefix_stripping() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
cap-drop = ["NET_RAW", "CAP_NET_RAW"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::CapabilityNameDuplicate {
                image,
                field,
                capability,
            } => {
                assert_eq!(image, "coding");
                assert_eq!(field, "cap-drop");
                assert_eq!(capability, "NET_RAW");
            }
            other => panic!("expected CapabilityNameDuplicate, got: {other:?}"),
        }
    }

    #[test]
    fn explicit_capability_drop_add_overlap_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
cap-drop = ["MKNOD"]
cap-add  = ["CAP_MKNOD"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::CapabilityDropAddConflict { image, capability } => {
                assert_eq!(image, "coding");
                assert_eq!(capability, "MKNOD");
            }
            other => panic!("expected CapabilityDropAddConflict, got: {other:?}"),
        }
    }

    #[test]
    fn empty_device_path_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
devices = ["   "]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::DevicePathEmpty { image } => {
                assert_eq!(image, "coding");
            }
            other => panic!("expected DevicePathEmpty, got: {other:?}"),
        }
    }

    #[test]
    fn relative_device_path_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
devices = ["dev/fuse"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::DevicePathRelative { image, device } => {
                assert_eq!(image, "coding");
                assert_eq!(device, "dev/fuse");
            }
            other => panic!("expected DevicePathRelative, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_device_path_errors() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[images.coding.security]
devices = ["/dev/fuse", "/dev/kvm", "/dev/fuse"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::DevicePathDuplicate { image, device } => {
                assert_eq!(image, "coding");
                assert_eq!(device, "/dev/fuse");
            }
            other => panic!("expected DevicePathDuplicate, got: {other:?}"),
        }
    }

    /// Sidecars reuse the whole `[security]` block, so the device rules reach
    /// them too -- and the error names the sidecar's scope, not the bare image.
    #[test]
    fn sidecar_device_path_errors_name_the_sidecar_scope() {
        let cfg = parse(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

[sidecars.tools]
image = "mcp-tools"

[sidecars.tools.security]
devices = ["dev/fuse"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::DevicePathRelative { image, device } => {
                assert_eq!(image, "sidecars.tools");
                assert_eq!(device, "dev/fuse");
            }
            other => panic!("expected DevicePathRelative, got: {other:?}"),
        }
    }

    #[test]
    fn top_level_tool_call_max_zero_errors() {
        let cfg = parse(
            r#"
tool-call-max = 0
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::ToolCallMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "top-level tool-call-max");
                assert_eq!(value, 0);
                assert_eq!(max, 2000);
            }
            other => panic!("expected ToolCallMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn agent_tool_call_max_too_large_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
tool-call-max = 5000
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::ToolCallMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "agents.coding.tool-call-max");
                assert_eq!(value, 5000);
                assert_eq!(max, 2000);
            }
            other => panic!("expected ToolCallMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn top_level_subagent_depth_max_zero_errors() {
        let cfg = parse(
            r#"
subagent-depth-max = 0
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SubagentDepthMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "top-level subagent-depth-max");
                assert_eq!(value, 0);
                assert_eq!(max, 16);
            }
            other => panic!("expected SubagentDepthMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn agent_subagent_depth_max_too_large_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
subagent-depth-max = 99
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SubagentDepthMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "agents.coding.subagent-depth-max");
                assert_eq!(value, 99);
                assert_eq!(max, 16);
            }
            other => panic!("expected SubagentDepthMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn subagent_depth_max_repo_overrides_global() {
        let global = parse(
            r#"
subagent-depth-max = 2
"#,
        );
        let repo = parse(
            r#"
subagent-depth-max = 4
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.subagent_depth_max, Some(4));
    }

    #[test]
    fn top_level_subagent_width_max_zero_errors() {
        let cfg = parse(
            r#"
subagent-width-max = 0
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SubagentWidthMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "top-level subagent-width-max");
                assert_eq!(value, 0);
                assert_eq!(max, 16);
            }
            other => panic!("expected SubagentWidthMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn agent_subagent_width_max_too_large_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
subagent-width-max = 99
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SubagentWidthMaxOutOfRange { path, value, max } => {
                assert_eq!(path, "agents.coding.subagent-width-max");
                assert_eq!(value, 99);
                assert_eq!(max, 16);
            }
            other => panic!("expected SubagentWidthMaxOutOfRange, got: {other:?}"),
        }
    }

    #[test]
    fn subagent_width_max_repo_overrides_global() {
        let global = parse(
            r#"
subagent-width-max = 2
"#,
        );
        let repo = parse(
            r#"
subagent-width-max = 4
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.subagent_width_max, Some(4));
    }

    #[test]
    fn top_level_retry_budget_secs_too_large_errors() {
        let cfg = parse(
            r#"
retry-budget-secs = 7200
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::RetryBudgetSecsTooLarge { path, value, max } => {
                assert_eq!(path, "top-level retry-budget-secs");
                assert_eq!(value, 7200);
                assert_eq!(max, 3600);
            }
            other => panic!("expected RetryBudgetSecsTooLarge, got: {other:?}"),
        }
    }

    #[test]
    fn provider_retry_budget_secs_too_large_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style             = "openai"
base-url          = "https://api.openai.com/v1"
api-key           = "${OPENAI_API_KEY}"
retry-budget-secs = 99999

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::RetryBudgetSecsTooLarge { path, value, max } => {
                assert_eq!(path, "providers.openai.retry-budget-secs");
                assert_eq!(value, 99999);
                assert_eq!(max, 3600);
            }
            other => panic!("expected RetryBudgetSecsTooLarge, got: {other:?}"),
        }
    }

    /// Zero is a value, not an omission: it turns retries off. The ceiling is
    /// one-sided precisely so this stays legal.
    #[test]
    fn zero_retry_budget_secs_is_accepted() {
        let cfg = parse(
            r#"
default-model = "fast"
retry-budget-secs = 0

[providers.openai]
style             = "openai"
base-url          = "https://api.openai.com/v1"
api-key           = "${OPENAI_API_KEY}"
retry-budget-secs = 0

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
"#,
        );
        cfg.validate(None)
            .expect("0 disables retries; it is not a range error");
        assert_eq!(cfg.retry_budget_secs, Some(0));
    }

    #[test]
    fn retry_budget_secs_repo_overrides_global() {
        let global = parse(
            r#"
retry-budget-secs = 60
"#,
        );
        let repo = parse(
            r#"
retry-budget-secs = 900
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.retry_budget_secs, Some(900));
    }

    #[test]
    fn retry_budget_secs_falls_back_to_global() {
        let global = parse(
            r#"
retry-budget-secs = 60
"#,
        );
        let merged = merge(global, parse(""));
        assert_eq!(merged.retry_budget_secs, Some(60));
    }

    #[test]
    fn top_level_tool_result_max_too_small_errors() {
        let cfg = parse(
            r#"
tool-result-max = 0
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::ToolResultMaxTooSmall { path, value, min } => {
                assert_eq!(path, "top-level tool-result-max");
                assert_eq!(value, 0);
                assert_eq!(min, 1024);
            }
            other => panic!("expected ToolResultMaxTooSmall, got: {other:?}"),
        }
    }

    #[test]
    fn agent_tool_result_max_too_large_errors() {
        let cfg = parse(
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
tool-result-max = 100000000
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::ToolResultMaxTooLarge { path, value, max } => {
                assert_eq!(path, "agents.coding.tool-result-max");
                assert_eq!(value, 100000000);
                assert_eq!(max, 16 * 1024 * 1024);
            }
            other => panic!("expected ToolResultMaxTooLarge, got: {other:?}"),
        }
    }
}

mod config_merge {
    use super::*;

    #[test]
    fn repo_overrides_global_by_name() {
        let global = parse(
            r#"
[providers.openai]
style    = "openai"
base-url = "https://global.example.com/v1"
api-key  = "${OPENAI_API_KEY}"
"#,
        );
        let repo = parse(
            r#"
[providers.openai]
style    = "openai"
base-url = "https://repo.example.com/v1"
api-key  = "${OPENAI_API_KEY}"
"#,
        );
        let merged = merge(global, repo);
        let LlmProvider::OpenAi { base_url, .. } = &merged.providers["openai"] else {
            panic!("expected OpenAi variant after merge");
        };
        assert_eq!(
            base_url, "https://repo.example.com/v1",
            "repo entry should win"
        );
    }

    /// Providers merge as whole enum values, not field by field: a repo entry
    /// may change the style of a global name and takes none of its fields
    /// along. Without that, a repo `anthropic` provider would inherit the
    /// global `openai` base URL for keys it happens not to restate.
    #[test]
    fn repo_provider_replaces_a_global_of_a_different_style() {
        let global = parse(
            r#"
[providers.claude]
style                = "openai"
base-url             = "https://openrouter.ai/api/v1"
api-key              = "${OPENROUTER_API_KEY}"
request-timeout-secs = 90
"#,
        );
        let repo = parse(
            r#"
[providers.claude]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"
"#,
        );
        let merged = merge(global, repo);
        let LlmProvider::Anthropic {
            base_url,
            api_key,
            request_timeout_secs,
            ..
        } = &merged.providers["claude"]
        else {
            panic!("repo entry should replace the global one wholesale");
        };
        assert_eq!(base_url, "https://api.anthropic.com");
        assert_eq!(api_key.var_name(), "ANTHROPIC_API_KEY");
        assert_eq!(
            *request_timeout_secs, None,
            "the global entry's timeout must not survive into the replacement"
        );
    }

    #[test]
    fn repo_keeps_global_entries_with_unique_names() {
        let global = parse(
            r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.anthropic]
style    = "anthropic"
base-url = "https://api.anthropic.com"
api-key  = "${ANTHROPIC_API_KEY}"
"#,
        );
        let repo = parse(
            r#"
[providers.staging]
style    = "openai"
base-url = "https://staging.example.com/v1"
api-key  = "${STAGING_API_KEY}"
"#,
        );
        let merged = merge(global, repo);
        assert!(merged.providers.contains_key("openai"));
        assert!(merged.providers.contains_key("anthropic"));
        assert!(merged.providers.contains_key("staging"));
    }

    #[test]
    fn model_cache_root_repo_overrides_global() {
        let global = parse(
            r#"
model-cache-root = "/var/cache/global/models"
"#,
        );
        let repo = parse(
            r#"
model-cache-root = "/var/cache/repo/models"
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(
            merged.model_cache_root.as_deref(),
            Some(Path::new("/var/cache/repo/models")),
        );
    }

    #[test]
    fn model_cache_root_global_used_when_repo_unset() {
        let global = parse(
            r#"
model-cache-root = "/var/cache/global/models"
"#,
        );
        let repo = parse("");
        let merged = merge(global, repo);
        assert_eq!(
            merged.model_cache_root.as_deref(),
            Some(Path::new("/var/cache/global/models")),
        );
    }

    #[test]
    fn scalar_repo_wins_over_global() {
        let global = parse(
            r#"
default-model = "fast-global"
"#,
        );
        let repo = parse(
            r#"
default-model = "fast-repo"
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.default_model.as_deref(), Some("fast-repo"));
    }

    #[test]
    fn scalar_global_used_when_repo_unset() {
        let global = parse(
            r#"
session-root = "/var/lib/outrig/sessions"
"#,
        );
        let repo = parse("");
        let merged = merge(global, repo);
        assert_eq!(
            merged.session_root.as_deref(),
            Some(Path::new("/var/lib/outrig/sessions")),
        );
    }

    #[test]
    fn tool_call_max_repo_overrides_global() {
        let global = parse(
            r#"
tool-call-max = 100
"#,
        );
        let repo = parse(
            r#"
tool-call-max = 300
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.tool_call_max, Some(300));
    }

    #[test]
    fn tool_call_max_global_used_when_repo_unset() {
        let global = parse(
            r#"
tool-call-max = 100
"#,
        );
        let repo = parse("");
        let merged = merge(global, repo);
        assert_eq!(merged.tool_call_max, Some(100));
    }

    #[test]
    fn tool_result_max_repo_overrides_global() {
        let global = parse(
            r#"
tool-result-max = 262144
"#,
        );
        let repo = parse(
            r#"
tool-result-max = 524288
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.tool_result_max, Some(524288));
    }

    #[test]
    fn tool_result_max_global_used_when_repo_unset() {
        let global = parse(
            r#"
tool-result-max = 262144
"#,
        );
        let repo = parse("");
        let merged = merge(global, repo);
        assert_eq!(merged.tool_result_max, Some(262144));
    }

    #[test]
    fn workspace_mounts_concatenate_global_then_repo() {
        let global = parse(
            r#"
[workspace]
host-path      = "/ignored/global/workspace"
container-path = "/ignored-global"

[[workspace.mounts]]
host-path      = "/global/docs"
container-path = "/resources/global-docs"
"#,
        );
        let repo = parse(
            r#"
[workspace]
host-path      = "."
container-path = "/workspace"

[[workspace.mounts]]
host-path      = "repo-cache"
container-path = "/resources/repo-cache"
access         = "read-write"
"#,
        );

        let merged = merge(global, repo);
        assert_eq!(merged.workspace.host_path, std::path::PathBuf::from("."));
        assert_eq!(
            merged.workspace.container_path,
            std::path::PathBuf::from("/workspace"),
        );
        assert_eq!(merged.workspace.mounts.len(), 2);
        assert_eq!(
            merged.workspace.mounts[0].container_path,
            std::path::PathBuf::from("/resources/global-docs"),
        );
        assert_eq!(merged.workspace.mounts[0].access, MountAccess::ReadOnly);
        assert_eq!(
            merged.workspace.mounts[1].container_path,
            std::path::PathBuf::from("/resources/repo-cache"),
        );
        assert_eq!(merged.workspace.mounts[1].access, MountAccess::ReadWrite);
    }

    #[test]
    fn repo_network_config_overrides_global_during_merge() {
        let global = parse(
            r#"
[network]
mode = "audit"
allow = ["github.com:443"]
"#,
        );
        let repo = parse(
            r#"
[network]
mode = "default"
"#,
        );
        let merged = merge(global, repo);
        assert_eq!(merged.network.mode, NetworkMode::Default);
        assert_eq!(
            merged.network.allow,
            vec![NetworkEntry::with_port("github.com", 443)],
        );
    }

    #[test]
    fn absent_repo_network_keeps_global_during_merge() {
        let global = parse(
            r#"
[network]
mode = "audit"
"#,
        );
        let repo = parse("");
        let merged = merge(global, repo);
        assert_eq!(merged.network.mode, NetworkMode::Audit);
    }
}

mod config_load {
    use super::*;

    /// End-to-end load of `tests/fixtures/config-full.toml` (acceptance criterion).
    /// Writes the fixture to a tempdir, plus the dockerfile/context paths it
    /// references, then drives the full disk pipeline through `Config::load`.
    #[test]
    fn fixture_loads_end_to_end() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(tmp.path(), FIXTURE_FULL);

        // The fixture's image references real paths under the repo root.
        let ctx = tmp.path().join(".agents/outrig/images/coding");
        fs::create_dir_all(&ctx).unwrap();
        fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();

        // The fixture's mistralrs `llama-local` model uses a relative
        // model-path; the existence check resolves against repo_root.
        let model_dir = tmp.path().join(".agents/outrig/models");
        fs::create_dir_all(&model_dir).unwrap();
        fs::write(model_dir.join("llama-3-8b-instruct.q4.gguf"), b"\0").unwrap();

        fs::create_dir_all(tmp.path().join(".agents/outrig/resources/docs")).unwrap();
        fs::create_dir_all(tmp.path().join(".agents/outrig/resources/cache")).unwrap();

        let cfg = Config::load(tmp.path(), None).expect("fixture loads end-to-end");
        assert_eq!(cfg.default_image.as_deref(), Some("coding"));
        assert_eq!(cfg.default_agent.as_deref(), Some("coding"));
        assert_eq!(cfg.default_model.as_deref(), Some("fast"));
    }

    #[test]
    fn missing_global_path_is_ok() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(tmp.path(), "");
        let absent = tmp.path().join("does-not-exist.toml");
        Config::load(tmp.path(), Some(&absent)).expect("absent global is treated as empty");
    }

    #[test]
    fn run_model_override_can_supply_selected_agents_missing_model() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
default-agent = "coding"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
preamble = "hi"
"#,
        );

        let strict_err = Config::load(tmp.path(), None).unwrap_err();
        assert!(
            matches!(
                expect_load_validation_err(strict_err),
                ConfigValidationError::AgentMissingModel { ref agent } if agent == "coding"
            ),
            "strict load should still reject a model-less agent with no default-model",
        );

        let cfg = Config::load_for_run(tmp.path(), None, None, Some("fast"))
            .expect("run --model supplies the selected agent model");
        assert_eq!(cfg.default_agent.as_deref(), Some("coding"));
    }

    #[test]
    fn run_model_override_does_not_supply_other_agents_missing_model() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
default-agent = "coding"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"

[agents.coding]
preamble = "hi"

[agents.review]
preamble = "review"
"#,
        );

        let err = Config::load_for_run(tmp.path(), None, None, Some("fast")).unwrap_err();
        assert!(
            matches!(
                expect_load_validation_err(err),
                ConfigValidationError::AgentMissingModel { ref agent } if agent == "review"
            ),
            "model override must only relax the selected agent",
        );
    }

    #[test]
    fn build_load_allows_agent_without_model_or_default() {
        let tmp = tempdir().unwrap();
        let image_dir = tmp.path().join("coding");
        fs::create_dir_all(&image_dir).unwrap();
        fs::write(image_dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
default-image = "coding"

[images.coding]
dockerfile = "coding/Dockerfile"
context    = "coding"

[agents.coding]
preamble = "hi"
"#,
        );

        let strict_err = Config::load(tmp.path(), None).unwrap_err();
        assert!(
            matches!(
                expect_load_validation_err(strict_err),
                ConfigValidationError::AgentMissingModel { ref agent } if agent == "coding"
            ),
            "strict load should still reject a model-less agent with no default-model",
        );

        let cfg = Config::load_for_build(tmp.path(), None)
            .expect("build load does not require an agent model or default-model");
        assert_eq!(cfg.default_image.as_deref(), Some("coding"));
        assert!(cfg.images.contains_key("coding"));
    }

    #[test]
    fn build_load_allows_dangling_default_model() {
        let tmp = tempdir().unwrap();
        let image_dir = tmp.path().join("coding");
        fs::create_dir_all(&image_dir).unwrap();
        fs::write(image_dir.join("Dockerfile"), "FROM scratch\n").unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
default-image = "coding"
default-model = "phantom"

[images.coding]
dockerfile = "coding/Dockerfile"
context    = "coding"
"#,
        );

        let strict_err = Config::load(tmp.path(), None).unwrap_err();
        assert!(
            matches!(
                expect_load_validation_err(strict_err),
                ConfigValidationError::UnknownDefaultModel { ref name } if name == "phantom"
            ),
            "strict load should still reject a dangling default-model",
        );

        let cfg = Config::load_for_build(tmp.path(), None)
            .expect("build load does not require default-model to resolve");
        assert_eq!(cfg.default_model.as_deref(), Some("phantom"));
        assert!(cfg.images.contains_key("coding"));
    }

    #[test]
    fn build_load_still_validates_image_paths() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
default-image = "coding"

[images.coding]
dockerfile = "missing/Dockerfile"
context    = "."

[agents.coding]
preamble = "hi"
"#,
        );

        let err = Config::load_for_build(tmp.path(), None).unwrap_err();
        assert!(
            matches!(
                expect_load_validation_err(err),
                ConfigValidationError::DockerfileMissing { ref image, .. } if image == "coding"
            ),
            "build load must still validate image paths",
        );
    }

    #[test]
    fn global_provides_default_model_for_repo_agent() {
        // Validates the merge+validate round-trip: global supplies the
        // default-model that the repo's agent (no explicit `model`) resolves
        // through.
        let tmp = tempdir().unwrap();
        let global_cfg = tmp.path().join("global.toml");
        fs::write(
            &global_cfg,
            r#"
default-model = "fast"

[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[models.fast]
provider   = "openai"
identifier = "gpt-4o-mini"
"#,
        )
        .unwrap();

        write_repo_cfg(
            tmp.path(),
            r#"
[agents.coding]
preamble = "hi"
"#,
        );

        let cfg = Config::load(tmp.path(), Some(&global_cfg))
            .expect("repo agent resolves through global default-model");
        assert!(cfg.agents.contains_key("coding"));
        assert!(cfg.models.contains_key("fast"));
    }

    #[test]
    fn global_network_mode_loads() {
        let tmp = tempdir().unwrap();
        let global_cfg = tmp.path().join("global.toml");
        fs::write(
            &global_cfg,
            r#"
[network]
mode = "audit"
"#,
        )
        .unwrap();
        write_repo_cfg(tmp.path(), "");

        let cfg = Config::load(tmp.path(), Some(&global_cfg)).expect("global network loads");
        assert_eq!(cfg.network.mode, NetworkMode::Audit);
    }

    #[test]
    fn repo_network_mode_loads() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
[network]
mode = "audit"
"#,
        );

        let cfg = Config::load(tmp.path(), None).expect("repo network should load");
        assert_eq!(cfg.network.mode, NetworkMode::Audit);
    }

    #[test]
    fn repo_network_mode_overrides_global() {
        let tmp = tempdir().unwrap();
        let global_cfg = tmp.path().join("global.toml");
        fs::write(
            &global_cfg,
            r#"
[network]
mode = "audit"
"#,
        )
        .unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
[network]
mode = "default"
"#,
        );

        let cfg = Config::load(tmp.path(), Some(&global_cfg))
            .expect("repo network should override global network");
        assert_eq!(cfg.network.mode, NetworkMode::Default);
    }

    #[test]
    fn repo_network_filter_mode_uses_global_policy() {
        let tmp = tempdir().unwrap();
        let global_cfg = tmp.path().join("global.toml");
        fs::write(
            &global_cfg,
            r#"
[network]
default = "deny"
allow = ["github.com:443"]
"#,
        )
        .unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
[network]
mode = "filter"
"#,
        );

        let cfg = Config::load(tmp.path(), Some(&global_cfg))
            .expect("repo mode should combine with global policy");
        assert_eq!(cfg.network.mode, NetworkMode::Filter);
        assert_eq!(
            cfg.network.allow,
            vec![NetworkEntry::with_port("github.com", 443)],
        );
    }

    #[test]
    fn repo_network_policy_keys_are_rejected() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(
            tmp.path(),
            r#"
[network]
mode = "filter"
allow = ["github.com:443"]
"#,
        );

        let err = Config::load(tmp.path(), None).unwrap_err();
        assert!(
            err.to_string()
                .contains("[network].allow belongs in global config"),
            "got: {err:?}",
        );
    }

    #[test]
    fn image_name_only_validates_clean() {
        let cfg = parse(
            r#"
[images.scratch]
image-name = "docker.io/library/ubuntu:24.04"
"#,
        );
        cfg.validate(None).expect("image-name-only image validates");
    }

    #[test]
    fn image_name_with_mcp_validates_clean() {
        let cfg = parse(
            r#"
[images.scratch]
image-name = "docker.io/library/ubuntu:24.04"

  [images.scratch.mcp]
  fs = { command = ["mcp-server-filesystem", "/workspace"] }
"#,
        );
        cfg.validate(None).expect("image-name with mcp validates");
    }

    #[test]
    fn container_source_missing_errors() {
        let cfg = parse(
            r#"
[images.empty]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageSourceMissing { .. }),
            "expected ImageSourceMissing, got: {err:?}"
        );
    }

    #[test]
    fn image_name_with_dockerfile_errors() {
        let cfg = parse(
            r#"
[images.bad]
image-name = "alpine:3.20"
dockerfile = "Dockerfile"
context    = "."
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageSourceConflict { .. }),
            "expected ImageSourceConflict, got: {err:?}"
        );
    }

    #[test]
    fn image_name_with_build_args_errors() {
        let cfg = parse(
            r#"
[images.bad]
image-name = "alpine:3.20"
build-args = { FOO = "bar" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageNameWithBuildArgs { .. }),
            "expected ImageNameWithBuildArgs, got: {err:?}"
        );
    }

    #[test]
    fn empty_image_name_errors() {
        let cfg = parse(
            r#"
[images.bad]
image-name = ""
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageNameEmpty { .. }),
            "expected ImageNameEmpty, got: {err:?}"
        );
    }

    #[test]
    fn dockerfile_without_context_errors() {
        let cfg = parse(
            r#"
[images.bad]
dockerfile = "Dockerfile"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageHalfBuilt { .. }),
            "expected ImageHalfBuilt, got: {err:?}"
        );
    }

    #[test]
    fn context_without_dockerfile_errors() {
        let cfg = parse(
            r#"
[images.bad]
context = "."
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::ImageHalfBuilt { .. }),
            "expected ImageHalfBuilt, got: {err:?}"
        );
    }
}

mod sidecar_config {
    use super::*;

    /// An `[images.coding]` block plus any top-level `[sidecars.<sc>]`
    /// blocks appended.
    fn image_block(rest: &str) -> Config {
        parse(&format!(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"
{rest}
"#
        ))
    }

    #[test]
    fn sidecar_block_parses_and_validates() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image      = "mcp-tools"
  workspace  = "ro"
  start      = "manual"
  on-failure = "warn"

  [[sidecars.tools.mounts]]
  host-path      = "cache"
  container-path = "/cache"
  access         = "read-write"

  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "tools" }
"#,
        );
        cfg.validate(None).expect("valid sidecar config");
        let sc = &cfg.sidecars["tools"];
        assert_eq!(sc.image, "mcp-tools");
        assert_eq!(sc.workspace, SidecarWorkspaceAccess::Ro);
        assert_eq!(sc.start, SidecarStart::Manual);
        assert_eq!(sc.on_failure, SidecarOnFailure::Warn);
        assert_eq!(sc.mounts.len(), 1);
        assert_eq!(cfg.images["coding"].mcp["fs"].sidecar(), Some("tools"));
    }

    #[test]
    fn sidecar_defaults_are_none_auto_abort() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"
"#,
        );
        let sc = &cfg.sidecars["tools"];
        assert_eq!(sc.workspace, SidecarWorkspaceAccess::None);
        assert_eq!(sc.start, SidecarStart::Auto);
        assert_eq!(sc.on_failure, SidecarOnFailure::Abort);
        assert!(sc.mounts.is_empty());
    }

    #[test]
    fn sidecar_unknown_key_rejected() {
        let err = Config::load_from_str(
            r#"
[images.coding]
dockerfile = "D"
context    = "ctx"

  [sidecars.tools]
  image   = "mcp-tools"
  restart = "always"
"#,
        )
        .expect_err("unknown sidecar key must be rejected");
        assert!(
            err.to_string().contains("restart"),
            "error should name the unknown key: {err}"
        );
    }

    #[test]
    fn sidecar_name_invalid_errors() {
        let cfg = image_block(
            r#"
  [sidecars."bad.name"]
  image = "mcp-tools"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SidecarNameInvalid { sidecar } => {
                assert_eq!(sidecar, "bad.name");
            }
            other => panic!("expected SidecarNameInvalid, got: {other:?}"),
        }
    }

    #[test]
    fn sidecar_name_may_start_with_digit() {
        let cfg = image_block(
            r#"
  [sidecars.9tools]
  image = "mcp-tools"
"#,
        );
        cfg.validate(None)
            .expect("digit-leading sidecar name is valid");
    }

    #[test]
    fn sidecar_image_empty_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "  "
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::SidecarImageEmpty { .. }),
            "expected SidecarImageEmpty, got: {err:?}"
        );
    }

    #[test]
    fn placement_conflict_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "tools", image = "other" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::McpPlacementConflict { .. }),
            "expected McpPlacementConflict, got: {err:?}"
        );
    }

    #[test]
    fn unknown_sidecar_reference_errors() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "nope" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::McpUnknownSidecar { sidecar, .. } => {
                assert_eq!(sidecar, "nope");
            }
            other => panic!("expected McpUnknownSidecar, got: {other:?}"),
        }
    }

    /// A named block whose one entry omits `command` is the named
    /// entrypoint-host form: that container's ENTRYPOINT is the server.
    #[test]
    fn named_sidecar_without_command_is_entrypoint_stdio() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
        );
        let spec = &cfg.images["coding"].mcp["fs"];
        assert!(!spec.has_command());
        assert!(spec.is_entrypoint_stdio());
        cfg.validate(None)
            .expect("named entrypoint-stdio form validates clean");
    }

    #[test]
    fn entrypoint_stdio_form_is_accepted() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fetch = { image = "ghcr.io/example/mcp-fetch:2", env = { TOKEN = "${FETCH_TOKEN}" } }
"#,
        );
        // Parse-level: Full form with no command.
        let spec = &cfg.images["coding"].mcp["fetch"];
        assert!(!spec.has_command());
        assert_eq!(spec.image(), Some("ghcr.io/example/mcp-fetch:2"));
        // Validation-level: the image's ENTRYPOINT is the server.
        cfg.validate(None)
            .expect("entrypoint-stdio form validates clean");
    }

    #[test]
    fn view_primary_named_block_validates_clean() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "docker.io/mcp/filesystem:latest"
  view  = "primary"

  [images.coding.mcp]
  fs = { sidecar = "tools", args = ["/workspace"] }
"#,
        );
        cfg.validate(None)
            .expect("view=primary entrypoint-stdio validates clean");
        assert_eq!(cfg.sidecars["tools"].view, SidecarView::Primary);
    }

    #[test]
    fn view_primary_inline_one_liner_validates_clean() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", view = "primary", args = ["/workspace"] }
"#,
        );
        cfg.validate(None)
            .expect("inline view=primary validates clean");
        assert_eq!(cfg.images["coding"].mcp["fs"].view(), SidecarView::Primary);
    }

    #[test]
    fn view_primary_with_workspace_rejected() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image     = "docker.io/mcp/filesystem:latest"
  view      = "primary"
  workspace = "ro"

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
        );
        match expect_validation_err(&cfg, None) {
            ConfigValidationError::SidecarViewWorkspaceConflict { sidecar } => {
                assert_eq!(sidecar, "tools");
            }
            other => panic!("expected SidecarViewWorkspaceConflict, got: {other:?}"),
        }
    }

    #[test]
    fn view_primary_with_drop_all_rejected() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "docker.io/mcp/filesystem:latest"
  view  = "primary"

  [sidecars.tools.security]
  capability-profile = "drop-all"

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
        );
        match expect_validation_err(&cfg, None) {
            ConfigValidationError::SidecarViewDropsCaps { sidecar } => {
                assert_eq!(sidecar, "tools");
            }
            other => panic!("expected SidecarViewDropsCaps, got: {other:?}"),
        }
    }

    #[test]
    fn view_primary_hosting_exec_stdio_rejected() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "docker.io/mcp/filesystem:latest"
  view  = "primary"

  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "tools" }
"#,
        );
        match expect_validation_err(&cfg, None) {
            ConfigValidationError::SidecarViewRequiresEntrypoint {
                sidecar, server, ..
            } => {
                assert_eq!(sidecar, "tools");
                assert_eq!(server, "fs");
            }
            other => panic!("expected SidecarViewRequiresEntrypoint, got: {other:?}"),
        }
    }

    #[test]
    fn inline_view_primary_with_command_rejected() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", command = ["mcp-fs"], view = "primary" }
"#,
        );
        match expect_validation_err(&cfg, None) {
            ConfigValidationError::McpViewNotInlineEntrypoint { server, .. } => {
                assert_eq!(server, "fs");
            }
            other => panic!("expected McpViewNotInlineEntrypoint, got: {other:?}"),
        }
    }

    #[test]
    fn inline_image_empty_errors() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { command = ["mcp-fs"], image = "" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::McpInlineImageEmpty { .. }),
            "expected McpInlineImageEmpty, got: {err:?}"
        );
    }

    #[test]
    fn anonymous_named_collision_errors() {
        let cfg = image_block(
            r#"
  [sidecars.grep]
  image = "mcp-tools"

  [images.coding.mcp]
  grep = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SidecarNameCollision { name, .. } => {
                assert_eq!(name, "grep");
            }
            other => panic!("expected SidecarNameCollision, got: {other:?}"),
        }
    }

    #[test]
    fn full_form_without_command_or_image_is_empty_command() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { env = { A = "b" } }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::EmptyMcpCommand { .. }),
            "expected EmptyMcpCommand, got: {err:?}"
        );
    }

    #[test]
    fn sidecar_mount_duplicate_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [[sidecars.tools.mounts]]
  host-path      = "a"
  container-path = "/cache"

  [[sidecars.tools.mounts]]
  host-path      = "b"
  container-path = "/cache"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::SidecarMount {
                    violation: MountRuleViolation::ContainerDuplicate { .. },
                    ..
                }
            ),
            "expected a duplicate-container-path SidecarMount, got: {err:?}"
        );
    }

    #[test]
    fn sidecar_mount_collides_with_enabled_workspace() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image     = "mcp-tools"
  workspace = "ro"

  [[sidecars.tools.mounts]]
  host-path      = "a"
  container-path = "/workspace"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(
                err,
                ConfigValidationError::SidecarMount {
                    violation: MountRuleViolation::ContainerDuplicate { .. },
                    ..
                }
            ),
            "expected a duplicate-container-path SidecarMount, got: {err:?}"
        );
    }

    #[test]
    fn sidecar_mount_on_workspace_path_ok_without_workspace_access() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [[sidecars.tools.mounts]]
  host-path      = "a"
  container-path = "/workspace"
"#,
        );
        cfg.validate(None)
            .expect("workspace path is free when workspace access is none");
    }

    #[test]
    fn sidecar_security_conflict_scopes_error_to_sidecar() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [sidecars.tools.security]
  cap-drop = ["NET_RAW"]
  cap-add  = ["NET_RAW"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::CapabilityDropAddConflict { image, capability } => {
                assert_eq!(image, "sidecars.tools");
                assert_eq!(capability, "NET_RAW");
            }
            other => panic!("expected CapabilityDropAddConflict, got: {other:?}"),
        }
    }

    #[test]
    fn placement_keys_round_trip_through_toml() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "tools" }
"#,
        );
        let rendered = toml::to_string(&cfg.images["coding"].mcp).expect("serialize");
        assert!(rendered.contains("sidecar = \"tools\""), "got: {rendered}");
        let back: std::collections::BTreeMap<String, McpServerSpec> =
            toml::from_str(&rendered).expect("round-trip");
        assert_eq!(back["fs"], cfg.images["coding"].mcp["fs"]);
    }

    // -- `args` on entrypoint-stdio servers ------------------------------

    #[test]
    fn args_on_inline_entrypoint_form_is_accepted() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
"#,
        );
        let spec = &cfg.images["coding"].mcp["fs"];
        assert!(spec.is_entrypoint_stdio());
        assert_eq!(spec.args(), ["/workspace"]);
        cfg.validate(None).expect("inline image + args validates");
    }

    #[test]
    fn args_on_named_entrypoint_host_from_the_block() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "docker.io/mcp/filesystem:latest"
  args  = ["/workspace"]

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
        );
        cfg.validate(None).expect("block-declared args validates");
    }

    #[test]
    fn args_on_named_entrypoint_host_from_the_entry() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "docker.io/mcp/filesystem:latest"

  [images.coding.mcp]
  fs = { sidecar = "tools", args = ["/workspace"] }
"#,
        );
        cfg.validate(None).expect("entry-declared args validates");
    }

    #[test]
    fn args_with_command_errors() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { command = ["mcp-fs"], image = "mcp-tools", args = ["/workspace"] }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::McpArgsWithCommand { image, server } => {
                assert_eq!(image, "coding");
                assert_eq!(server, "fs");
            }
            other => panic!("expected McpArgsWithCommand, got: {other:?}"),
        }
    }

    #[test]
    fn args_without_placement_errors() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { args = ["/workspace"] }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::McpArgsWithoutPlacement { image, server } => {
                assert_eq!(image, "coding");
                assert_eq!(server, "fs");
            }
            other => panic!("expected McpArgsWithoutPlacement, got: {other:?}"),
        }
    }

    #[test]
    fn args_declared_on_both_entry_and_block_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"
  args  = ["/a"]

  [images.coding.mcp]
  fs = { sidecar = "tools", args = ["/b"] }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::McpArgsDeclaredTwice {
                image,
                server,
                sidecar,
            } => {
                assert_eq!(image, "coding");
                assert_eq!(server, "fs");
                assert_eq!(sidecar, "tools");
            }
            other => panic!("expected McpArgsDeclaredTwice, got: {other:?}"),
        }
    }

    #[test]
    fn sidecar_args_without_entrypoint_server_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"
  args  = ["/workspace"]

  [images.coding.mcp]
  fs = { command = ["mcp-fs"], sidecar = "tools" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SidecarArgsWithoutEntrypoint { sidecar } => {
                assert_eq!(sidecar, "tools");
            }
            other => panic!("expected SidecarArgsWithoutEntrypoint, got: {other:?}"),
        }
    }

    #[test]
    fn empty_args_list_is_accepted_and_elided() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { image = "mcp-tools", args = [] }
"#,
        );
        cfg.validate(None).expect("empty args validates");
        let rendered = toml::to_string(&cfg.images["coding"].mcp).expect("serialize");
        assert!(!rendered.contains("args"), "args should elide: {rendered}");
    }

    #[test]
    fn args_round_trip_without_collapsing_to_short() {
        let cfg = image_block(
            r#"
  [images.coding.mcp]
  fs = { image = "docker.io/mcp/filesystem:latest", args = ["/workspace"] }
"#,
        );
        let rendered = toml::to_string(&cfg.images["coding"].mcp).expect("serialize");
        assert!(rendered.contains("\"/workspace\""), "got: {rendered}");
        let back: std::collections::BTreeMap<String, McpServerSpec> =
            toml::from_str(&rendered).expect("round-trip");
        assert!(
            matches!(back["fs"], McpServerSpec::Full { .. }),
            "must stay Full, got: {:?}",
            back["fs"]
        );
        assert_eq!(back["fs"], cfg.images["coding"].mcp["fs"]);
    }

    // -- named entrypoint hosts -------------------------------------------

    #[test]
    fn named_entrypoint_host_with_a_second_server_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"

  [images.coding.mcp]
  also = { command = ["mcp-grep"], sidecar = "tools" }
  fs   = { sidecar = "tools" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SidecarEntrypointNotAlone {
                image,
                sidecar,
                server,
                other,
            } => {
                assert_eq!(image, "coding");
                assert_eq!(sidecar, "tools");
                assert_eq!(server, "fs");
                assert_eq!(other, "also");
            }
            other => panic!("expected SidecarEntrypointNotAlone, got: {other:?}"),
        }
    }

    #[test]
    fn named_entrypoint_host_with_manual_start_errors() {
        let cfg = image_block(
            r#"
  [sidecars.tools]
  image = "mcp-tools"
  start = "manual"

  [images.coding.mcp]
  fs = { sidecar = "tools" }
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::SidecarEntrypointNotAuto {
                image,
                sidecar,
                server,
            } => {
                assert_eq!(image, "coding");
                assert_eq!(sidecar, "tools");
                assert_eq!(server, "fs");
            }
            other => panic!("expected SidecarEntrypointNotAuto, got: {other:?}"),
        }
    }
}

/// Relative paths resolve against the directory of the file that declared them,
/// not against whichever repo happens to be current. Every test here points
/// `--global-config` at a tempdir, which is what makes the global-config
/// behavior testable without touching a real `$HOME`.
/// Relative paths resolve against the directory of the file that declared them,
/// not against whichever repo happens to be current. Every test here points
/// `--global-config` at a tempdir, which is what makes the global-config
/// behavior testable without touching a real `$HOME`.
mod config_path_provenance {
    use super::*;

    /// An empty repo and a global config holding `body`. Returns
    /// `(repo_tmp, global_tmp, global_config_path)`; the tempdirs must stay
    /// alive for the duration of the test.
    fn repo_and_global(body: &str) -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        write_repo_cfg(repo.path(), "");
        let global_cfg = write_global_cfg(global.path(), body);
        (repo, global, global_cfg)
    }

    /// The same, plus an `images/x/` project beside the global config -- the
    /// `~/.outrig/images/<name>/` shape, from a repo somewhere else entirely.
    fn global_image_project() -> (tempfile::TempDir, tempfile::TempDir, std::path::PathBuf) {
        let (repo, global, global_cfg) = repo_and_global(
            r#"
[images.x]
dockerfile = "images/x/Dockerfile"
context    = "images/x"
"#,
        );
        let proj = global.path().join("images/x");
        fs::create_dir_all(&proj).unwrap();
        fs::write(proj.join("Dockerfile"), "FROM scratch\n").unwrap();
        (repo, global, global_cfg)
    }

    #[test]
    fn global_image_build_paths_resolve_against_global_dir() {
        let (repo, global, global_cfg) = global_image_project();

        let cfg = Config::load(repo.path(), Some(&global_cfg))
            .expect("a global build-shape image must validate from its own directory");

        let image = &cfg.images["x"];
        let src = image
            .config_source()
            .expect("a loaded entry carries its source");
        assert_eq!(src.base_dir(), global.path());
        assert_eq!(src.config_path(), global_cfg);
        assert_eq!(
            image.resolved_build_paths(repo.path()),
            (
                global.path().join("images/x/Dockerfile"),
                global.path().join("images/x"),
            ),
        );
    }

    /// The same paths under the repo root do *not* exist, which is what made
    /// this shape unusable before: the entry was legal to write and impossible
    /// to load.
    #[test]
    fn global_image_paths_are_not_looked_for_under_the_repo() {
        let (repo, _global, global_cfg) = global_image_project();
        assert!(
            !repo.path().join("images/x/Dockerfile").exists(),
            "the fixture must not accidentally satisfy the old repo-root rule",
        );

        Config::load(repo.path(), Some(&global_cfg))
            .expect("resolution must not consult the repo root at all");
    }

    /// Reading the Dockerfile and taring the context is pure filesystem work --
    /// no podman, no buildah -- so the build path is reachable from an ungated
    /// test up to the point where an image would actually be produced.
    #[tokio::test]
    async fn global_image_tag_computes_from_the_global_dir() {
        let (repo, _global, global_cfg) = global_image_project();
        let cfg = Config::load(repo.path(), Some(&global_cfg)).expect("global image config loads");

        outrig::image::compute_tag_for("x", &cfg.images["x"], repo.path())
            .await
            .expect("cache key must read the Dockerfile from the global directory");
    }

    #[test]
    fn global_dockerfile_missing_names_the_global_config() {
        let (repo, _global, global_cfg) = repo_and_global(
            r#"
[images.x]
dockerfile = "images/x/Dockerfile"
context    = "images/x"
"#,
        );

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        assert!(
            err.to_string().contains("declared in"),
            "the rendered message must carry the clause: {err}",
        );
        match err {
            ConfigValidationError::DockerfileMissing {
                image,
                path,
                declared_in,
                ..
            } => {
                assert_eq!(image, "x");
                assert_eq!(
                    path,
                    std::path::PathBuf::from("images/x/Dockerfile"),
                    "the reported path stays the raw config value",
                );
                assert_eq!(
                    declared_in,
                    Some(global_cfg),
                    "a global entry's failure must not read as a repo problem",
                );
            }
            other => panic!("expected DockerfileMissing, got: {other:?}"),
        }
    }

    #[test]
    fn global_context_missing_names_the_global_config() {
        let (repo, global, global_cfg) = repo_and_global(
            r#"
[images.x]
dockerfile = "Dockerfile"
context    = "missing-ctx"
"#,
        );
        // Dockerfile exists beside the global config; only the context is gone.
        fs::write(global.path().join("Dockerfile"), "FROM scratch\n").unwrap();

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        match err {
            ConfigValidationError::ContextMissing {
                path, declared_in, ..
            } => {
                assert_eq!(path, std::path::PathBuf::from("missing-ctx"));
                assert_eq!(declared_in, Some(global_cfg));
            }
            other => panic!("expected ContextMissing, got: {other:?}"),
        }
    }

    /// The concatenated case. `merge` splices the global and repo mount lists
    /// into one `Vec`, so a single base directory provably cannot be right for
    /// every element -- each entry has to carry its own.
    #[test]
    fn concatenated_mounts_resolve_against_their_own_files() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();

        fs::create_dir_all(global.path().join("shared")).unwrap();
        fs::create_dir_all(repo.path().join("local")).unwrap();

        let global_cfg = write_global_cfg(
            global.path(),
            r#"
[[workspace.mounts]]
host-path      = "shared"
container-path = "/shared"
"#,
        );
        write_repo_cfg(
            repo.path(),
            r#"
[[workspace.mounts]]
host-path      = "local"
container-path = "/local"
"#,
        );

        let cfg = Config::load(repo.path(), Some(&global_cfg))
            .expect("each mount must resolve against the file that declared it");

        let mounts = &cfg.workspace.mounts;
        assert_eq!(mounts.len(), 2, "global mounts precede repo mounts");
        assert_eq!(
            mounts[0].resolved_host_path(repo.path()),
            global.path().join("shared"),
        );
        assert_eq!(
            mounts[1].resolved_host_path(repo.path()),
            repo.path().join("local"),
        );
    }

    /// The negative twin: satisfying a global mount's path under the *repo*
    /// root must not make it validate. Before this change it would have.
    ///
    /// It is also the case whose message was misleading: `host-path "shared"
    /// does not exist` is true and useless when `shared` was never meant to be
    /// found under the repo in the first place.
    #[test]
    fn global_mount_is_not_satisfied_by_a_repo_path() {
        let (repo, _global, global_cfg) = repo_and_global(
            r#"
[[workspace.mounts]]
host-path      = "shared"
container-path = "/shared"
"#,
        );
        // Only the repo has `shared/`; the global config's own directory doesn't.
        fs::create_dir_all(repo.path().join("shared")).unwrap();

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        assert!(
            err.to_string().contains("declared in"),
            "the rendered message must carry the clause: {err}",
        );
        match err {
            ConfigValidationError::WorkspaceMountHostMissing {
                path, declared_in, ..
            } => {
                assert_eq!(
                    path,
                    std::path::PathBuf::from("shared"),
                    "the reported path stays the raw config value",
                );
                assert_eq!(
                    declared_in,
                    Some(global_cfg),
                    "a bare relative path reads as a repo problem without this",
                );
            }
            other => panic!("expected WorkspaceMountHostMissing, got: {other:?}"),
        }
    }

    /// The concatenated list is the case one base directory provably cannot
    /// cover, so attribution has to be per entry. One load reports one failure
    /// -- `check_mount_list` returns on the first -- so this breaks each side in
    /// turn: whichever entry is bad names *its own* file, not whichever config
    /// was loaded last.
    #[test]
    fn concatenated_mount_failures_name_their_own_files() {
        const GLOBAL_BODY: &str = r#"
[[workspace.mounts]]
host-path      = "shared"
container-path = "/shared"
"#;
        const REPO_BODY: &str = r#"
[[workspace.mounts]]
host-path      = "local"
container-path = "/local"
"#;

        // Only the repo entry is satisfied, so the global one is what fails.
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        fs::create_dir_all(repo.path().join("local")).unwrap();
        write_repo_cfg(repo.path(), REPO_BODY);
        let global_cfg = write_global_cfg(global.path(), GLOBAL_BODY);

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        match err {
            ConfigValidationError::WorkspaceMountHostMissing {
                path, declared_in, ..
            } => {
                assert_eq!(path, std::path::PathBuf::from("shared"));
                assert_eq!(declared_in, Some(global_cfg));
            }
            other => panic!("expected the global entry to fail, got: {other:?}"),
        }

        // The mirror, which is what rules out "whichever file loaded last":
        // only the global entry is satisfied now.
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        fs::create_dir_all(global.path().join("shared")).unwrap();
        write_repo_cfg(repo.path(), REPO_BODY);
        let global_cfg = write_global_cfg(global.path(), GLOBAL_BODY);

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        match err {
            ConfigValidationError::WorkspaceMountHostMissing {
                path, declared_in, ..
            } => {
                assert_eq!(path, std::path::PathBuf::from("local"));
                assert_eq!(
                    declared_in,
                    Some(repo.path().join(".agents/outrig/config.toml")),
                );
            }
            other => panic!("expected the repo entry to fail, got: {other:?}"),
        }
    }

    /// A config that never went through `Config::load` records no source, and
    /// must render no clause at all -- not an empty one, not a `None`. Asserted
    /// once, on the whole rendered string, because `declared_in_clause` is
    /// shared by every variant that carries the field.
    #[test]
    fn a_config_without_a_source_renders_no_clause() {
        let tmp = tempdir().unwrap();
        let cfg = parse(
            r#"
[[workspace.mounts]]
host-path      = "missing-docs"
container-path = "/resources/docs"
"#,
        );

        let err = expect_validation_err(&cfg, Some(tmp.path()));
        assert_eq!(
            err.to_string(),
            r#"workspace mount host-path "missing-docs" does not exist"#,
        );
        assert!(matches!(
            err,
            ConfigValidationError::WorkspaceMountHostMissing {
                declared_in: None,
                ..
            }
        ));
    }

    /// The wrapping boundary: `validate_sidecar` maps the violation whole into
    /// `SidecarMount`, so the clause has to survive as part of the violation's
    /// own rendering rather than being restated by the wrapper.
    #[test]
    fn global_sidecar_mount_failure_names_the_global_config() {
        let (repo, _global, global_cfg) = repo_and_global(
            r#"
[sidecars.tools]
image = "docker.io/library/alpine:3"

  [[sidecars.tools.mounts]]
  host-path      = "gh-config"
  container-path = "/gh"
"#,
        );
        // Only the repo has it, so resolving against the global dir must fail.
        fs::create_dir_all(repo.path().join("gh-config")).unwrap();

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        let rendered = err.to_string();
        assert!(
            rendered.contains(&format!("{global_cfg:?}")),
            "the clause must reach the wrapped rendering: {rendered}",
        );
        match err {
            ConfigValidationError::SidecarMount {
                sidecar,
                violation: MountRuleViolation::HostMissing { declared_in, .. },
            } => {
                assert_eq!(sidecar, "tools");
                assert_eq!(declared_in, Some(global_cfg));
            }
            other => panic!("expected a host-missing SidecarMount, got: {other:?}"),
        }
    }

    /// A container-path rule is about the value, not about where a host
    /// directory was looked for -- but the question the clause answers ("which
    /// file do I go edit") is the same, and the concatenated list is what makes
    /// the bare path ambiguous.
    #[test]
    fn global_mount_container_path_violation_names_the_global_config() {
        let (repo, _global, global_cfg) = repo_and_global(
            r#"
[[workspace.mounts]]
host-path      = "shared"
container-path = "relative/path"
"#,
        );

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        match err {
            ConfigValidationError::WorkspaceMountContainerNotAbsolute {
                path, declared_in, ..
            } => {
                assert_eq!(path, std::path::PathBuf::from("relative/path"));
                assert_eq!(declared_in, Some(global_cfg));
            }
            other => panic!("expected WorkspaceMountContainerNotAbsolute, got: {other:?}"),
        }
    }

    /// A duplicate spans two files by construction. The clause names the
    /// *rejected* entry -- the repo one, since global mounts are concatenated
    /// first -- rather than claiming to name both sides of the collision.
    #[test]
    fn duplicate_container_path_names_the_rejected_entry() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        fs::create_dir_all(global.path().join("shared")).unwrap();
        fs::create_dir_all(repo.path().join("also-shared")).unwrap();

        let global_cfg = write_global_cfg(
            global.path(),
            r#"
[[workspace.mounts]]
host-path      = "shared"
container-path = "/shared"
"#,
        );
        write_repo_cfg(
            repo.path(),
            r#"
[[workspace.mounts]]
host-path      = "also-shared"
container-path = "/shared"
"#,
        );

        let err =
            expect_load_validation_err(Config::load(repo.path(), Some(&global_cfg)).unwrap_err());
        match err {
            ConfigValidationError::WorkspaceMountContainerDuplicate {
                path, declared_in, ..
            } => {
                assert_eq!(path, std::path::PathBuf::from("/shared"));
                assert_eq!(
                    declared_in,
                    Some(repo.path().join(".agents/outrig/config.toml")),
                    "the second entry is the one refused, so it is the one to edit",
                );
            }
            other => panic!("expected WorkspaceMountContainerDuplicate, got: {other:?}"),
        }
    }

    /// A sidecar block is replaced whole by `merge`, so its mount list is always
    /// single-origin -- but that origin can still be the global file.
    #[test]
    fn global_sidecar_mounts_resolve_against_the_global_dir() {
        let (repo, global, global_cfg) = repo_and_global(
            r#"
[images.x]
image-name = "docker.io/library/alpine:3"

[sidecars.tools]
image = "docker.io/library/alpine:3"

  [[sidecars.tools.mounts]]
  host-path      = "gh-config"
  container-path = "/gh"
"#,
        );
        fs::create_dir_all(global.path().join("gh-config")).unwrap();

        let cfg = Config::load(repo.path(), Some(&global_cfg))
            .expect("a global sidecar mount resolves beside the global config");
        assert_eq!(
            cfg.sidecars["tools"].mounts[0].resolved_host_path(repo.path()),
            global.path().join("gh-config"),
        );
    }

    /// Repo-declared entries keep resolving exactly as before, and a hand-built
    /// entry that never saw `Config::load` records no source and falls back to
    /// the passed root. That fallback is what keeps every existing library
    /// caller correct.
    #[test]
    fn repo_entries_and_sourceless_entries_use_the_repo_root() {
        let repo = tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".agents/outrig/images/coding")).unwrap();
        fs::write(
            repo.path().join(".agents/outrig/images/coding/Dockerfile"),
            "FROM scratch\n",
        )
        .unwrap();
        write_repo_cfg(
            repo.path(),
            r#"
[images.coding]
dockerfile = ".agents/outrig/images/coding/Dockerfile"
context    = ".agents/outrig/images/coding"
"#,
        );

        let cfg = Config::load(repo.path(), None).expect("repo config loads unchanged");
        assert_eq!(cfg.images["coding"].base_dir(repo.path()), repo.path());
        assert_eq!(
            cfg.images["coding"]
                .config_source()
                .map(ConfigSource::config_path),
            Some(repo.path().join(".agents/outrig/config.toml")),
        );

        let hand_built = ImageConfig::from_dockerfile("Dockerfile", ".");
        assert!(hand_built.config_source().is_none());
        assert_eq!(hand_built.base_dir(repo.path()), repo.path());

        let hand_built_mount = MountConfig::new("data", "/data", MountAccess::ReadOnly);
        assert!(hand_built_mount.config_source().is_none());
        assert_eq!(
            hand_built_mount.resolved_host_path(repo.path()),
            repo.path().join("data"),
        );
    }

    /// Absolute paths ignore the base directory entirely, whichever file they
    /// came from. This is also what keeps `Outrig::launch`'s empty-root call
    /// site a no-op.
    #[test]
    fn absolute_paths_ignore_the_declaring_directory() {
        let repo = tempdir().unwrap();
        let global = tempdir().unwrap();
        let abs = global.path().join("abs-mount");
        fs::create_dir_all(&abs).unwrap();

        let global_cfg = write_global_cfg(
            global.path(),
            &format!(
                r#"
[[workspace.mounts]]
host-path      = {abs:?}
container-path = "/abs"
"#,
            ),
        );
        write_repo_cfg(repo.path(), "");

        let cfg = Config::load(repo.path(), Some(&global_cfg)).expect("absolute host path loads");
        assert_eq!(
            cfg.workspace.mounts[0].resolved_host_path(Path::new("")),
            abs,
        );
    }
}
