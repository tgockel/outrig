use serde::Serialize;

use crate::config::Config;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BaseFamily {
    Debian,
    Alpine,
}

pub fn validate_dockerfile(dockerfile: &str) -> DockerfileValidation {
    let mut warnings = Vec::new();
    let mut last_cmd: Option<(usize, String)> = None;
    let mut base_family = None;

    for (idx, raw) in dockerfile.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if starts_with_instruction(line, "FROM") {
            base_family = from_image(line).and_then(known_family);
        } else if starts_with_instruction(line, "USER") {
            warnings.push(ValidationMessage {
                code: "user_ignored",
                message: "OutRig bootstraps and runs containers as the host UID/GID, so Dockerfile USER directives are not respected.".to_string(),
                line: Some(line_no),
            });
        } else if starts_with_instruction(line, "CMD") {
            last_cmd = Some((line_no, line.to_string()));
        }
    }

    match last_cmd {
        Some((line, cmd)) if !is_sleep_infinity_cmd(&cmd) => {
            warnings.push(ValidationMessage {
                code: "cmd_may_exit",
                message: "A CMD that does not run forever can let the container exit before the agent has finished using it; OutRig templates use CMD [\"sleep\", \"infinity\"].".to_string(),
                line: Some(line),
            });
        }
        None => warnings.push(ValidationMessage {
            code: "cmd_missing",
            message: "Without a long-running CMD, the container may exit before the agent has finished using it; OutRig templates use CMD [\"sleep\", \"infinity\"].".to_string(),
            line: None,
        }),
        _ => {}
    }

    match base_family {
        Some(BaseFamily::Debian) if !contains_token(dockerfile, "passwd") => {
            warnings.push(ValidationMessage {
                code: "user_bootstrap_package_missing",
                message: "Known Debian-family images usually need the passwd package so useradd/groupadd are available for OutRig's host-UID bootstrap.".to_string(),
                line: None,
            });
        }
        Some(BaseFamily::Alpine) if !contains_token(dockerfile, "shadow") => {
            warnings.push(ValidationMessage {
                code: "user_bootstrap_package_missing",
                message: "Known Alpine-family images usually need the shadow package so useradd/groupadd are available for OutRig's host-UID bootstrap.".to_string(),
                line: None,
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

fn starts_with_instruction(line: &str, instruction: &str) -> bool {
    let Some(head) = line.split_ascii_whitespace().next() else {
        return false;
    };
    head.eq_ignore_ascii_case(instruction)
}

fn from_image(line: &str) -> Option<&str> {
    let mut words = line.split_ascii_whitespace();
    let from = words.next()?;
    if !from.eq_ignore_ascii_case("FROM") {
        return None;
    }
    words.find(|word| !word.starts_with("--"))
}

fn known_family(image: &str) -> Option<BaseFamily> {
    let lower = image.to_ascii_lowercase();
    let leaf = lower.rsplit('/').next().unwrap_or(&lower);
    if leaf.starts_with("alpine:") || leaf == "alpine" || leaf.contains("-alpine") {
        Some(BaseFamily::Alpine)
    } else if leaf.starts_with("debian:")
        || leaf == "debian"
        || leaf.starts_with("ubuntu:")
        || leaf == "ubuntu"
        || leaf.contains("bookworm")
        || (leaf.starts_with("python:") && leaf.contains("slim"))
    {
        Some(BaseFamily::Debian)
    } else {
        None
    }
}

fn is_sleep_infinity_cmd(line: &str) -> bool {
    let Some(cmd) = line.get(3..) else {
        return false;
    };
    let normalized: String = cmd.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    normalized == "[\"sleep\",\"infinity\"]"
}

fn contains_token(text: &str, needle: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .any(|token| token == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_template_has_no_warnings() {
        let out = validate_dockerfile(
            r#"
FROM docker.io/library/debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends passwd
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
RUN apt-get install -y passwd
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
RUN apt-get install -y passwd
CMD ["bash"]
"#,
        );
        assert!(out.warnings.iter().any(|w| w.code == "cmd_may_exit"));
    }

    #[test]
    fn known_base_missing_user_package_warns() {
        let out = validate_dockerfile(
            r#"
FROM alpine:latest
CMD ["sleep", "infinity"]
"#,
        );
        assert!(
            out.warnings
                .iter()
                .any(|w| w.code == "user_bootstrap_package_missing"),
            "expected user bootstrap warning: {:?}",
            out.warnings,
        );
    }

    #[test]
    fn unknown_base_does_not_warn_about_user_package() {
        let out = validate_dockerfile(
            r#"
FROM registry.example.com/team/custom-dev:latest
CMD ["sleep", "infinity"]
"#,
        );
        assert_eq!(out.warnings, Vec::new());
    }

    #[test]
    fn config_validator_reports_invalid_mcp_name() {
        let out = validate_config(
            r#"
[containers.c]
dockerfile = "Dockerfile"
context = "."

[containers.c.mcp]
"bad.name" = ["mcp"]
"#,
        );
        assert!(!out.valid);
        assert!(out.errors[0].message.contains("invalid mcp server name"));
    }
}
