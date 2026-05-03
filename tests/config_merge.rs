//! Integration tests for `Config::validate`, `merge`, and `Config::load`.
//! Mirrors every rule in `doc/reference/config.md`'s "Validation rules" plus
//! the merge semantics documented in "Resolution: which file wins".

use std::fs;
use std::path::Path;

use tempfile::tempdir;

use outrig::config::{Config, ConfigValidationError, LlmProvider, McpServerSpec, merge};
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

mod config_validate {
    use super::*;

    #[test]
    fn all_good_validates_clean() {
        let cfg = parse(FIXTURE_FULL);
        cfg.validate(None).expect("fixture validates structurally");
    }

    #[test]
    fn dangling_default_container_errors() {
        let cfg = parse(
            r#"
default-container = "missing"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        assert!(
            matches!(err, ConfigValidationError::UnknownDefaultContainer { ref name } if name == "missing"),
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
container = "ghost"
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::UnknownAgentContainer { agent, container } => {
                assert_eq!(agent, "coding");
                assert_eq!(container, "ghost");
            }
            other => panic!("expected UnknownAgentContainer, got: {other:?}"),
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
[containers.coding]
dockerfile = "D"
context    = "ctx"

  [containers.coding.mcp]
  "bad name" = ["bin"]
"#,
        );
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::InvalidMcpServerName { container, server } => {
                assert_eq!(container, "coding");
                assert_eq!(server, "bad name");
            }
            other => panic!("expected InvalidMcpServerName, got: {other:?}"),
        }
    }

    #[test]
    fn mcp_server_name_leading_dash_errors() {
        let cfg = parse(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"

  [containers.coding.mcp]
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
    fn mcp_command_empty_errors() {
        let cfg = parse(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"

  [containers.coding.mcp]
  srv = []
"#,
        );
        // Sanity: shape is Short with empty command.
        assert!(matches!(
            cfg.containers["coding"].mcp["srv"],
            McpServerSpec::Short(ref v) if v.is_empty(),
        ));
        let err = expect_validation_err(&cfg, None);
        match err {
            ConfigValidationError::EmptyMcpCommand { container, server } => {
                assert_eq!(container, "coding");
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
[containers.coding]
dockerfile = "Dockerfile"
context    = "."
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        match err {
            ConfigValidationError::DockerfileMissing { container, path } => {
                assert_eq!(container, "coding");
                assert_eq!(path, std::path::PathBuf::from("Dockerfile"));
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
[containers.coding]
dockerfile = "Dockerfile"
context    = "missing-ctx"
"#,
        );
        let err = expect_validation_err(&cfg, Some(tmp.path()));
        match err {
            ConfigValidationError::ContextMissing { container, path } => {
                assert_eq!(container, "coding");
                assert_eq!(path, std::path::PathBuf::from("missing-ctx"));
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
[containers.coding]
dockerfile = "does/not/exist/Dockerfile"
context    = "does/not/exist"
"#,
        );
        cfg.validate(None)
            .expect("structural-only validate ignores disk paths");
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

    #[test]
    fn repo_keeps_global_entries_with_unique_names() {
        let global = parse(
            r#"
[providers.openai]
style    = "openai"
base-url = "https://api.openai.com/v1"
api-key  = "${OPENAI_API_KEY}"

[providers.anthropic]
style    = "openai"
base-url = "https://api.anthropic.com/v1"
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
}

mod config_load {
    use super::*;

    fn write_repo_cfg(root: &Path, body: &str) {
        let agents = root.join(".agents/outrig");
        fs::create_dir_all(&agents).unwrap();
        fs::write(agents.join("config.toml"), body).unwrap();
    }

    /// End-to-end load of `tests/fixtures/config-full.toml` (acceptance criterion).
    /// Writes the fixture to a tempdir, plus the dockerfile/context paths it
    /// references, then drives the full disk pipeline through `Config::load`.
    #[test]
    fn fixture_loads_end_to_end() {
        let tmp = tempdir().unwrap();
        write_repo_cfg(tmp.path(), FIXTURE_FULL);

        // The fixture's container references real paths under the repo root.
        let ctx = tmp.path().join(".agents/outrig/containers/coding");
        fs::create_dir_all(&ctx).unwrap();
        fs::write(ctx.join("Dockerfile"), "FROM scratch\n").unwrap();

        let cfg = Config::load(tmp.path(), None).expect("fixture loads end-to-end");
        assert_eq!(cfg.default_container.as_deref(), Some("coding"));
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
}
