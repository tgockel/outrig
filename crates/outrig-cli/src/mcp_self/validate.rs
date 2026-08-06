use serde::Serialize;

use outrig::config::Config;
use outrig::container::embedded::parse_standalone_image_toml;

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

pub fn validate_config(toml: &str) -> ConfigValidation {
    match Config::load_from_str(toml).and_then(|cfg| cfg.validate(None).map(|()| cfg)) {
        Ok(_) => ConfigValidation {
            valid: true,
            errors: Vec::new(),
        },
        Err(err) => ConfigValidation {
            valid: false,
            errors: vec![ValidationMessage {
                code: "config_invalid",
                message: err.to_string(),
                line: None,
            }],
        },
    }
}

pub fn validate_image_toml(toml: &str) -> ConfigValidation {
    match parse_standalone_image_toml(toml) {
        Ok(_) => ConfigValidation {
            valid: true,
            errors: Vec::new(),
        },
        Err(err) => ConfigValidation {
            valid: false,
            errors: vec![ValidationMessage {
                code: "image_toml_invalid",
                message: err.to_string(),
                line: None,
            }],
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
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("invalid mcp server name"));
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
