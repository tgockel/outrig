//! Connect config-declared URL servers for the external-client MCP proxy, using the
//! Session's approval, validation and HTTP placement without starting an interpreter.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use super::mcp::{McpDeclaredIn, TrustAnchor, in_the_clear, url_misconfigured};
use super::mcp_placement::{Http, Launch, Placement, Server, Unconnected, UrlLaunch};
use crate::error::{McpSessionError, OutrigError};
use crate::mcp::{McpClient, STARTUP_TIMEOUT};

/// Connect a config-declared URL for the external-client proxy, without creating a Session.
pub(crate) async fn connect_proxy_url(
    image: &crate::config::ImageConfig,
    image_name: &str,
    name: &str,
    repo_root: &std::path::Path,
    approver: Arc<dyn super::Approver>,
) -> crate::error::Result<McpClient> {
    let bad = |why: String| OutrigError::Configuration(format!("mcp server {name:?}: {why}"));
    let spec = image
        .mcp()
        .get(name)
        .filter(|spec| spec.is_url())
        .ok_or_else(|| bad("it names no URL".to_string()))?;
    let entry = crate::outrig_::ConfiguredUrl {
        spec: spec.clone(),
        trust_anchor: spec
            .trust_anchor()
            .map(|path| crate::config::resolve_against(image.base_dir(repo_root), path)),
        table: format!("images.{image_name}.mcp.{name}"),
        declared_in: image.declared_in(),
        unapproved: None,
    };
    let launch = UrlLaunch {
        url: spec.url().unwrap_or_default().to_string(),
        trust_anchor: entry.trust_anchor.clone().map(TrustAnchor::Path),
        trust: spec.trust(),
        allow_stateless: spec.allow_stateless().unwrap_or(true),
    };
    if let Some(why) = url_misconfigured(&launch, spec.headers(), McpDeclaredIn::Config, &[]) {
        return Err(bad(why));
    }
    // Approval precedes both secret resolution and any host-network request. It uses the
    // Session's exact digest and store, so approval in either command approves the other.
    if image.repo_declared() {
        let asked = super::approval::mcp_url_request(name, &entry, repo_root);
        super::approval::ask(
            &asked,
            super::approval::Store::user().as_ref(),
            Some(&approver),
        )
        .await
        .map_err(|why| bad(why.to_string()))?;
    }
    let server = Server {
        name: name.to_string(),
        declared: Some("config"),
        launch: Launch::Url(Box::new(launch)),
        env: crate::mcp::resolve_mcp_env(name, spec.headers().clone(), &BTreeMap::new())?,
    };
    if let Some(why) = in_the_clear(spec.url().unwrap_or_default()) {
        tracing::warn!(target: "outrig::mcp_proxy", server = name, "{why}");
    }
    let client = Http::new(Vec::new())
        .connect(&server, STARTUP_TIMEOUT)
        .await
        .map_err(|failure| match failure {
            Unconnected::Outrig(e) => e,
            Unconnected::Http(failed) => {
                OutrigError::McpStartupFailed(Box::new(crate::error::McpStartupFailure {
                    name: name.to_string(),
                    declaration_source: Some("config".to_string()),
                    command: "streamable HTTP".to_string(),
                    exit: "(no process)".to_string(),
                    stderr_path: PathBuf::new(),
                    stderr_tail: "(none: OutRig runs no process of this server)".to_string(),
                    kind: failed.source.kind,
                    source: Box::new(McpSessionError::new(
                        failed.source.kind,
                        failed.source.message.clone(),
                    )),
                    classified: failed.source,
                    class: failed.class,
                }))
            }
        })?;
    Ok(
        client.with_call_timeout(crate::config::effective_mcp_call_timeout(
            spec.call_timeout_secs(),
            None,
        )),
    )
}
