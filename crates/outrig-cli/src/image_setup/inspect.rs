//! `outrig image inspect` -- read an image's OutRig labels.

use std::collections::BTreeMap;

use outrig::container::embedded::{StandaloneImageLabels, parse_standalone_image_labels};
use outrig::image::{self, ImageTag};

use crate::error::{OutrigError, Result};

/// Inspect an image's declared OutRig config from OCI labels. This is
/// metadata-only: no pull, no container start, and no MCP server initialization.
/// By default this reads only the local image store; `remote` switches to a
/// registry metadata read via skopeo.
pub async fn run(image_ref: &str, remote: bool) -> Result<()> {
    let labels = if remote {
        image::read_remote_image_labels(image_ref).await?
    } else {
        read_local_image_labels(image_ref).await?
    };
    let parsed = parse_standalone_image_labels(image_ref, &labels)?;
    print!("{}", render_inspect(image_ref, &parsed));
    Ok(())
}

async fn read_local_image_labels(image_ref: &str) -> Result<BTreeMap<String, String>> {
    let tag = ImageTag(image_ref.to_string());
    if !image::probe_pulled(&tag).await? {
        return Err(OutrigError::Configuration(format!(
            "local image {image_ref:?} not found; `outrig image inspect` is local-only and does not pull"
        ))
        .into());
    }
    Ok(image::read_image_labels(&tag, None).await?)
}

pub fn render_inspect(image_ref: &str, labels: &StandaloneImageLabels) -> String {
    let mut out = String::new();
    out.push_str("image: ");
    out.push_str(image_ref);
    out.push('\n');

    if let Some(description) = &labels.description {
        out.push_str("description: ");
        out.push_str(description);
        out.push('\n');
    }
    if let Some(version) = &labels.version {
        out.push_str("version: ");
        out.push_str(version);
        out.push('\n');
    }
    if !labels.tags.is_empty() {
        out.push_str("tags: ");
        out.push_str(&json(&labels.tags));
        out.push('\n');
    }

    if let Some(mcp) = &labels.mcp {
        out.push_str("mcp:\n");
        for (server, spec) in mcp {
            let (command, env) = spec.normalize();
            out.push_str("  ");
            out.push_str(server);
            out.push_str(":\n");
            out.push_str("    command: ");
            out.push_str(&json(&command));
            out.push('\n');

            if !env.is_empty() {
                out.push_str("    env:\n");
                for (key, value) in env {
                    out.push_str("      ");
                    out.push_str(&key);
                    out.push_str(": ");
                    out.push_str(&json(&value));
                    out.push('\n');
                }
            }
        }
    }

    out
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("inspect values serialize to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use outrig::config::{EnvValue, McpServerSpec};

    #[test]
    fn render_inspect_prints_metadata_commands_and_env() {
        let mut env = BTreeMap::new();
        env.insert(
            "CARGO_HOME".to_string(),
            EnvValue::Literal("/cache".to_string()),
        );
        env.insert(
            "TOKEN".to_string(),
            EnvValue::EnvRef("DB_TOKEN".to_string()),
        );

        let mut mcp = BTreeMap::new();
        mcp.insert(
            "build".to_string(),
            McpServerSpec::Full {
                command: vec!["cargo-mcp".to_string(), "--stdio".to_string()],
                env,
            },
        );
        mcp.insert(
            "fs".to_string(),
            McpServerSpec::Short(vec![
                "mcp-server-filesystem".to_string(),
                "/workspace".to_string(),
            ]),
        );

        let labels = StandaloneImageLabels {
            description: Some("Rust tooling".to_string()),
            version: Some("0.1.0".to_string()),
            tags: vec!["rust".to_string(), "build".to_string()],
            mcp: Some(mcp),
        };

        assert_eq!(
            render_inspect("rust-dev", &labels),
            concat!(
                "image: rust-dev\n",
                "description: Rust tooling\n",
                "version: 0.1.0\n",
                "tags: [\"rust\",\"build\"]\n",
                "mcp:\n",
                "  build:\n",
                "    command: [\"cargo-mcp\",\"--stdio\"]\n",
                "    env:\n",
                "      CARGO_HOME: \"/cache\"\n",
                "      TOKEN: \"${DB_TOKEN}\"\n",
                "  fs:\n",
                "    command: [\"mcp-server-filesystem\",\"/workspace\"]\n",
            )
        );
    }

    #[test]
    fn render_inspect_omits_absent_metadata_and_mcp() {
        let labels = StandaloneImageLabels {
            description: None,
            version: None,
            tags: Vec::new(),
            mcp: None,
        };

        assert_eq!(render_inspect("plain", &labels), "image: plain\n");
    }
}
