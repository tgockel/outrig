use serde::Serialize;

use outrig::config::Config;
use outrig::container::embedded::parse_standalone_image_toml;

use crate::mcp_self::args::ConfigScope;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationMessage {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DockerfileValidation {
    pub warnings: Vec<ValidationMessage>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigValidation {
    pub valid: bool,
    pub errors: Vec<ValidationMessage>,
    /// What the load would set aside rather than fail on -- a key outrig does
    /// not know, which `outrig run` warns about and ignores. Never makes the
    /// config invalid on its own.
    pub warnings: Vec<ValidationMessage>,
}

pub fn validate_dockerfile(dockerfile: &str) -> DockerfileValidation {
    let mut warnings = Vec::new();
    let mut last_cmd: Option<(usize, String)> = None;

    for (idx, raw) in dockerfile.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if starts_with_instruction(line, "USER") {
            warnings.push(ValidationMessage {
                code: "user_ignored",
                message: "OutRig bootstraps and runs containers as the host UID/GID, so Dockerfile USER directives are not respected.".to_string(),
                line: Some(line_no),
            });
        } else if starts_with_instruction(line, "ENTRYPOINT") {
            warnings.push(ValidationMessage {
                code: "entrypoint_takes_args",
                message: "OutRig appends `sleep infinity` after the image reference to keep the container alive. podman appends trailing arguments to an exec-form ENTRYPOINT rather than replacing it, so this image would run `<entrypoint> sleep infinity`. Leave ENTRYPOINT unset on any image OutRig starts this way -- the primary, and any sidecar whose servers are exec'd into it. An entrypoint-stdio sidecar image, whose ENTRYPOINT is the server itself, is a separate case and is fine.".to_string(),
                line: Some(line_no),
            });
        } else if starts_with_instruction(line, "CMD") {
            last_cmd = Some((line_no, line.to_string()));
        }
    }

    match last_cmd {
        Some((line, cmd)) if !is_sleep_infinity_cmd(&cmd) => {
            warnings.push(ValidationMessage {
                code: "cmd_ignored",
                message: "OutRig appends `sleep infinity` after the image reference, which overrides this CMD -- it has no effect on an OutRig session. OutRig templates still use CMD [\"sleep\", \"infinity\"] so the image behaves the same under a plain `podman run`.".to_string(),
                line: Some(line),
            });
        }
        _ => {}
    }

    DockerfileValidation { warnings }
}

/// Validate a config fragment as the file `scope` names.
///
/// Two checks, because a config load is two checks. `Config::validate` is the
/// one that runs on the *merged* config and so cannot tell which file declared
/// what; `Config::validate_as_repo` is the one `Config::load` applies to the
/// repo file alone, and it refuses the keys that describe the operator's own
/// machine -- `session-root`, `model-cache-root`, `[network]`'s policy keys,
/// `[repo].env-allow`, and a provider carrying an `api-key`. Running only the
/// first reported a repo fragment with `[network] allow = [...]` as valid and
/// left `outrig run` to refuse it, which is the worst place to learn it: after
/// the user has installed the file an agent said was good.
///
/// Repo-side first, as `Config::load` orders them, so a fragment that breaks
/// both rules reports the error that would stop it first.
pub fn validate_config(toml: &str, scope: ConfigScope) -> ConfigValidation {
    let (warnings, result) = match Config::load_from_str(toml) {
        // `load_from_str` sets aside nothing but unknown keys.
        Ok(cfg) => (
            cfg.warnings()
                .iter()
                .map(|w| ValidationMessage {
                    code: "unknown_config_key",
                    message: format!("unknown key `{}`, ignored", w.key()),
                    line: Some(w.line()),
                })
                .collect(),
            repo_scope_result(&cfg, scope).and_then(|()| {
                cfg.validate(None).map_err(|err| ValidationMessage {
                    code: "config_invalid",
                    message: err.to_string(),
                    line: None,
                })
            }),
        ),
        Err(err) => (
            Vec::new(),
            Err(ValidationMessage {
                code: "config_invalid",
                message: err.to_string(),
                line: None,
            }),
        ),
    };
    match result {
        Ok(()) => ConfigValidation {
            valid: true,
            errors: Vec::new(),
            warnings,
        },
        Err(error) => ConfigValidation {
            valid: false,
            errors: vec![error],
            warnings,
        },
    }
}

/// The repo-only rules, skipped for a fragment written for the global config
/// -- which is where every key they refuse belongs, so holding a global
/// fragment to them would refuse exactly the file that is allowed to say it.
///
/// The error carries its own code, so a caller that asked for the default
/// scope can tell "this is not a valid config" from "this is not valid *in a
/// repo config*", and the message says which argument moves it.
fn repo_scope_result(cfg: &Config, scope: ConfigScope) -> Result<(), ValidationMessage> {
    if scope == ConfigScope::Global {
        return Ok(());
    }
    cfg.validate_as_repo().map_err(|err| ValidationMessage {
        code: "repo_config_invalid",
        message: format!(
            "{err}; pass scope = \"global\" to check this fragment against the \
             global config's rules instead"
        ),
        line: None,
    })
}

pub fn validate_image_toml(toml: &str) -> ConfigValidation {
    match parse_standalone_image_toml(toml) {
        Ok(_) => ConfigValidation {
            valid: true,
            errors: Vec::new(),
            warnings: Vec::new(),
        },
        Err(err) => ConfigValidation {
            valid: false,
            errors: vec![ValidationMessage {
                code: "image_toml_invalid",
                message: err.to_string(),
                line: None,
            }],
            warnings: Vec::new(),
        },
    }
}

fn starts_with_instruction(line: &str, instruction: &str) -> bool {
    let Some(head) = line.split_ascii_whitespace().next() else {
        return false;
    };
    head.eq_ignore_ascii_case(instruction)
}

fn is_sleep_infinity_cmd(line: &str) -> bool {
    let Some(cmd) = line.get(3..) else {
        return false;
    };
    let normalized: String = cmd.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    normalized == "[\"sleep\",\"infinity\"]"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_template_has_no_warnings() {
        let out = validate_dockerfile(
            r#"
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends curl
WORKDIR /workspace
CMD ["sleep", "infinity"]
"#,
        );
        assert_eq!(out.warnings, Vec::new());
    }

    #[test]
    fn user_directive_is_advisory_warning() {
        let out = validate_dockerfile(
            r#"
FROM debian:bookworm-slim
RUN apt-get install -y curl
USER app
CMD ["sleep", "infinity"]
"#,
        );
        assert_eq!(out.warnings[0].code, "user_ignored");
        assert_eq!(out.warnings[0].line, Some(4));
    }

    #[test]
    fn non_forever_cmd_is_warning() {
        let out = validate_dockerfile(
            r#"
FROM debian:bookworm-slim
RUN apt-get install -y curl
CMD ["bash"]
"#,
        );
        assert!(out.warnings.iter().any(|w| w.code == "cmd_ignored"));
    }

    /// A missing `CMD` is not a problem: OutRig supplies the command itself,
    /// and the e2e suite's shell-less fixture is an image with no `CMD` at all.
    #[test]
    fn absent_cmd_is_not_a_warning() {
        let out = validate_dockerfile(
            r#"
FROM debian:bookworm-slim
RUN apt-get install -y curl
"#,
        );
        assert_eq!(out.warnings, Vec::new());
    }

    /// The instruction that actually breaks a primary image: podman appends
    /// the `sleep infinity` OutRig supplies to it rather than replacing it.
    #[test]
    fn entrypoint_is_warning() {
        let out = validate_dockerfile(
            r#"
FROM debian:bookworm-slim
ENTRYPOINT ["/usr/bin/myserver"]
CMD ["sleep", "infinity"]
"#,
        );
        let warning = out
            .warnings
            .iter()
            .find(|w| w.code == "entrypoint_takes_args")
            .expect("an ENTRYPOINT warning");
        assert_eq!(warning.line, Some(3));
    }

    #[test]
    fn config_validator_reports_invalid_mcp_name() {
        let out = validate_config(
            r#"
[images.c]
dockerfile = "Dockerfile"
context = "."

[images.c.mcp]
"bad.name" = ["mcp"]
"#,
            ConfigScope::Repo,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("invalid mcp server name"));
    }

    /// The key `outrig run` would warn about and ignore is a warning here
    /// too, with its line, and the fragment is still valid.
    #[test]
    fn config_validator_warns_on_an_unknown_key() {
        let out = validate_config(
            r#"
[images.c]
image-name = "docker.io/library/debian:stable"
contex     = "."
"#,
            ConfigScope::Repo,
        );
        assert!(out.valid, "an unknown key alone is not invalid: {out:?}");
        assert_eq!(
            out.warnings,
            [ValidationMessage {
                code: "unknown_config_key",
                message: "unknown key `images.c.contex`, ignored".to_string(),
                line: Some(4),
            }],
        );
    }

    /// A misspelled required key fails the fragment, and the error names the
    /// misspelling rather than the key it stood for.
    #[test]
    fn config_validator_names_a_misspelled_required_key() {
        let out = validate_config(
            r#"
[[workspace.mounts]]
host-pth       = "/a"
container-path = "/a"
"#,
            ConfigScope::Repo,
        );
        assert!(!out.valid, "{out:?}");
        assert!(
            out.errors[0].message.contains("unknown field `host-pth`"),
            "{out:?}"
        );
    }

    /// The regression: a repo fragment carrying the operator's own keys was
    /// reported valid, because only the merged-config rules were run. `outrig
    /// run` then refused the file the agent had just told the user to install.
    #[test]
    fn config_validator_refuses_a_repo_fragment_that_sets_network_policy() {
        let out = validate_config(
            r#"
[network]
mode  = "filter"
allow = ["github.com:443"]
"#,
            ConfigScope::Repo,
        );
        assert!(!out.valid, "{out:?}");
        assert_eq!(out.errors[0].code, "repo_config_invalid", "{out:?}");
        assert!(
            out.errors[0].message.contains("[network].allow"),
            "the error should name the key: {out:?}"
        );
        assert!(
            out.errors[0].message.contains("scope"),
            "and say which argument moves the fragment: {out:?}"
        );
    }

    /// Each of the repo-only rules, so a fifth one added to
    /// `validate_as_repo` is reachable here by construction rather than by
    /// having been remembered.
    #[test]
    fn config_validator_refuses_every_repo_only_key() {
        for fragment in [
            "session-root = \"/srv/outrig\"\n",
            "model-cache-root = \"/srv/models\"\n",
            "[network]\ndefault = \"deny\"\n",
            "[repo]\nenv-allow = []\n",
            "[providers.p]\nstyle = \"openai\"\n\
             base-url = \"https://api.openai.com/v1\"\n\
             api-key = \"${OPENAI_API_KEY}\"\n",
        ] {
            let out = validate_config(fragment, ConfigScope::Repo);
            assert!(!out.valid, "{fragment:?} should be repo-invalid: {out:?}");
            assert_eq!(out.errors[0].code, "repo_config_invalid", "{out:?}");

            let global = validate_config(fragment, ConfigScope::Global);
            assert!(
                global.valid,
                "{fragment:?} is exactly what the global config is for: {global:?}"
            );
        }
    }

    /// `scope = "global"` skips the repo-only rules and nothing else: the
    /// cross-reference rules every config is held to still run.
    #[test]
    fn global_scope_still_runs_the_shared_rules() {
        let out = validate_config(
            r#"
[models.m]
provider   = "nope"
identifier = "gpt-4o"
"#,
            ConfigScope::Global,
        );
        assert!(!out.valid, "{out:?}");
        assert_eq!(out.errors[0].code, "config_invalid", "{out:?}");
    }

    /// An image block -- what this tool is mostly handed -- is valid in
    /// either file, so the scope argument costs the common case nothing.
    #[test]
    fn an_image_block_is_valid_in_both_scopes() {
        let fragment = r#"
[images.c]
image-name = "docker.io/library/debian:stable"

[images.c.mcp]
fs = ["mcp-server-filesystem", "/workspace"]
"#;
        for scope in [ConfigScope::Repo, ConfigScope::Global] {
            let out = validate_config(fragment, scope);
            assert!(out.valid, "{scope:?}: {out:?}");
        }
    }

    #[test]
    fn image_toml_validator_accepts_explicit_build() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"
description = "Rust tools"
version = "0.1.0"
tags = ["rust"]

[build]
dockerfile = "Dockerfile"
context = "."

[mcp]
fs = { command = ["mcp-server-filesystem", "/workspace"] }
"#,
        );
        assert!(out.valid, "expected valid image.toml: {out:?}");
        assert_eq!(out.errors, Vec::new());
    }

    #[test]
    fn image_toml_validator_accepts_default_build() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"

[mcp]
fs = ["mcp-server-filesystem", "/workspace"]
"#,
        );
        assert!(
            out.valid,
            "expected valid default build image.toml: {out:?}"
        );
    }

    #[test]
    fn image_toml_validator_rejects_missing_image_ref() {
        let out = validate_image_toml(
            r#"
[image]
description = "missing ref"

[mcp]
fs = ["mcp-server-filesystem", "/workspace"]
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("image.ref is required"));
    }

    #[test]
    fn image_toml_validator_rejects_partial_build_fields() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"

[build]
context = "."

[mcp]
fs = ["mcp-server-filesystem", "/workspace"]
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("build.dockerfile"));
    }

    #[test]
    fn image_toml_validator_rejects_empty_mcp() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"

[mcp]
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("mcp table"));
    }

    #[test]
    fn image_toml_validator_rejects_invalid_mcp_name() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"

[mcp]
"bad.name" = ["mcp"]
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("invalid mcp server name"));
    }

    #[test]
    fn image_toml_validator_rejects_empty_mcp_command() {
        let out = validate_image_toml(
            r#"
[image]
ref = "rust-dev"

[mcp]
fs = []
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("empty command"));
    }
}
