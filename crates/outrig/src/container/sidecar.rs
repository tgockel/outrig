//! Session MCP placement planning.
//!
//! Turns an `[images.<name>]` block into a [`SessionMcpPlan`]: which sidecar
//! containers the session wants, and which container hosts each MCP server.
//! The plan is pure data -- image resolution, container starts, and MCP
//! connections happen in the caller (`session_setup` in the CLI) -- so the
//! merge and collision rules here are unit-testable without podman.
//!
//! Merge order is deterministic: config entries first (they win wholesale,
//! placement included), then the primary image's `org.outrig.mcp` label, then
//! each named sidecar's label in name order. A name two labels both declare,
//! with no config override, is a hard error -- the per-session server
//! namespace is flat.

use std::collections::BTreeMap;

use crate::config::{
    ContainerSecurity, ImageConfig, McpServerSpec, MountConfig, SidecarOnFailure, SidecarStart,
    SidecarWorkspaceAccess,
};
use crate::container::embedded::McpDeclarationSource;
use crate::error::{OutrigError, Result};

/// Which container hosts an MCP server. Anonymous sidecars are keyed by
/// their declaring server's name, so `Sidecar` covers both forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    Primary,
    Sidecar(String),
}

impl Placement {
    /// Human-facing host description, e.g. for `show-merged` and errors.
    pub fn description(&self) -> String {
        match self {
            Self::Primary => "primary".to_string(),
            Self::Sidecar(name) => format!("sidecar {name:?}"),
        }
    }
}

/// One merged MCP server: its spec, where it was declared, and which
/// container it runs in.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedServer {
    pub spec: McpServerSpec,
    pub source: McpDeclarationSource,
    pub placement: Placement,
}

/// One sidecar container the session wants, named or anonymous. `image` is
/// unresolved -- an `[images.<name>]` config name or a raw podman ref.
#[derive(Debug, Clone, PartialEq)]
pub struct SidecarPlan {
    pub name: String,
    pub image: String,
    pub workspace: SidecarWorkspaceAccess,
    pub start: SidecarStart,
    pub on_failure: SidecarOnFailure,
    pub mounts: Vec<MountConfig>,
    pub security: ContainerSecurity,
    /// Anonymous sidecars come from an inline `image` key on one MCP entry;
    /// they take all defaults and their image's `org.outrig.mcp` label is
    /// inert (exactly the declaring server runs there).
    pub anonymous: bool,
}

impl SidecarPlan {
    /// The all-defaults plan an inline `image` key implies.
    fn anonymous(server_name: &str, image: &str) -> Self {
        Self {
            name: server_name.to_string(),
            image: image.to_string(),
            workspace: SidecarWorkspaceAccess::None,
            start: SidecarStart::Auto,
            on_failure: SidecarOnFailure::Abort,
            mounts: Vec::new(),
            security: ContainerSecurity::default(),
            anonymous: true,
        }
    }
}

/// The session's full MCP placement plan.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionMcpPlan {
    /// Flat per-session server namespace, fully merged.
    pub servers: BTreeMap<String, PlacedServer>,
    /// Sidecars by name -- named blocks plus anonymous ones.
    pub sidecars: BTreeMap<String, SidecarPlan>,
}

impl SessionMcpPlan {
    /// Servers placed in the named sidecar, in name order.
    pub fn servers_in(&self, sidecar: &str) -> impl Iterator<Item = (&String, &PlacedServer)> {
        self.servers.iter().filter(
            move |(_, placed)| matches!(&placed.placement, Placement::Sidecar(sc) if sc == sidecar),
        )
    }

    /// Whether the sidecar needs the in-container user bootstrap: it hosts at
    /// least one exec-stdio server (exec needs `--user` and `HOME`), sees the
    /// workspace, or declares mounts. A mount-less entrypoint-stdio sidecar
    /// keeps the image's own `USER` untouched.
    pub fn sidecar_needs_bootstrap(&self, sidecar: &SidecarPlan) -> bool {
        self.servers_in(&sidecar.name)
            .any(|(_, placed)| placed.spec.has_command())
            || sidecar.workspace != SidecarWorkspaceAccess::None
            || !sidecar.mounts.is_empty()
    }
}

/// Build the config-declared half of the plan: named sidecar blocks,
/// anonymous sidecars from inline `image` keys, and every `[images.<x>.mcp]`
/// entry with its placement. Assumes the config has passed validation.
pub fn plan_from_config(image_cfg: &ImageConfig) -> SessionMcpPlan {
    let mut plan = SessionMcpPlan::default();

    for (name, sidecar) in &image_cfg.sidecars {
        plan.sidecars.insert(
            name.clone(),
            SidecarPlan {
                name: name.clone(),
                image: sidecar.image.clone(),
                workspace: sidecar.workspace,
                start: sidecar.start,
                on_failure: sidecar.on_failure,
                mounts: sidecar.mounts.clone(),
                security: sidecar.security.clone(),
                anonymous: false,
            },
        );
    }

    for (name, spec) in &image_cfg.mcp {
        let placement = if let Some(sc) = spec.sidecar() {
            Placement::Sidecar(sc.to_string())
        } else if let Some(image) = spec.image() {
            plan.sidecars
                .insert(name.clone(), SidecarPlan::anonymous(name, image));
            Placement::Sidecar(name.clone())
        } else {
            Placement::Primary
        };
        plan.servers.insert(
            name.clone(),
            PlacedServer {
                spec: spec.clone(),
                source: McpDeclarationSource::ConfigToml,
                placement,
            },
        );
    }

    plan
}

/// Merge the primary image's `org.outrig.mcp` label into the plan. Config
/// entries win wholesale (placement included), so label entries whose name is
/// already planned are dropped.
pub fn merge_primary_labels(plan: &mut SessionMcpPlan, image_mcp: BTreeMap<String, McpServerSpec>) {
    for (name, spec) in image_mcp {
        plan.servers.entry(name).or_insert(PlacedServer {
            spec,
            source: McpDeclarationSource::ImageLabel,
            placement: Placement::Primary,
        });
    }
}

/// Merge a named sidecar's `org.outrig.mcp` label into the plan: its servers
/// materialize as exec-stdio servers *in that sidecar*. Config overrides by
/// name (whole entry, placement included). A leftover name that another
/// container's label already claims is a startup error -- the namespace is
/// flat. Never called for anonymous sidecars; their labels are inert.
pub fn merge_sidecar_labels(
    plan: &mut SessionMcpPlan,
    sidecar: &str,
    label_mcp: BTreeMap<String, McpServerSpec>,
) -> Result<()> {
    for (name, spec) in label_mcp {
        if let Some(existing) = plan.servers.get(&name) {
            match existing.source {
                // Config replaces the whole entry, including placement.
                McpDeclarationSource::ConfigToml | McpDeclarationSource::LaunchSpec => continue,
                McpDeclarationSource::ImageLabel => {
                    return Err(OutrigError::Configuration(format!(
                        "duplicate mcp server name {name:?}: declared by the image labels of \
                         both {} and sidecar {sidecar:?}; override the entry by name in \
                         [images.<name>.mcp] to pick one",
                        existing.placement.description(),
                    )));
                }
            }
        }
        let (command, env) = spec.normalize();
        plan.servers.insert(
            name,
            PlacedServer {
                // Rewrite so serialization (e.g. `show-merged`) shows the
                // placement the label materialized into.
                spec: McpServerSpec::Full {
                    command: Some(command),
                    env,
                    sidecar: Some(sidecar.to_string()),
                    image: None,
                },
                source: McpDeclarationSource::ImageLabel,
                placement: Placement::Sidecar(sidecar.to_string()),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_cfg(toml_src: &str) -> ImageConfig {
        toml::from_str(toml_src).expect("image config parses")
    }

    fn short(cmd: &[&str]) -> McpServerSpec {
        McpServerSpec::Short(cmd.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn plan_places_servers_by_config_keys() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[mcp]
local = ["mcp-local"]
fs    = { command = ["mcp-fs"], sidecar = "tools" }
grep  = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
"#,
        );
        let plan = plan_from_config(&cfg);

        assert_eq!(plan.servers["local"].placement, Placement::Primary);
        assert_eq!(
            plan.servers["fs"].placement,
            Placement::Sidecar("tools".to_string())
        );
        assert_eq!(
            plan.servers["grep"].placement,
            Placement::Sidecar("grep".to_string())
        );

        assert!(!plan.sidecars["tools"].anonymous);
        let grep = &plan.sidecars["grep"];
        assert!(grep.anonymous);
        assert_eq!(grep.image, "ghcr.io/example/mcp-grep:1");
        assert_eq!(grep.workspace, SidecarWorkspaceAccess::None);
        assert_eq!(grep.on_failure, SidecarOnFailure::Abort);
        assert_eq!(grep.start, SidecarStart::Auto);
    }

    #[test]
    fn primary_label_entries_yield_to_config() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[mcp]
fs = { command = ["config-fs"], sidecar = "tools" }
"#,
        );
        let mut plan = plan_from_config(&cfg);
        merge_primary_labels(
            &mut plan,
            BTreeMap::from([
                ("fs".to_string(), short(&["label-fs"])),
                ("lint".to_string(), short(&["label-lint"])),
            ]),
        );

        // Config kept its spec AND its placement.
        assert_eq!(
            plan.servers["fs"].placement,
            Placement::Sidecar("tools".to_string())
        );
        assert_eq!(plan.servers["fs"].source, McpDeclarationSource::ConfigToml);
        // Label-only entry landed in the primary.
        assert_eq!(plan.servers["lint"].placement, Placement::Primary);
        assert_eq!(
            plan.servers["lint"].source,
            McpDeclarationSource::ImageLabel
        );
    }

    #[test]
    fn sidecar_label_entries_are_scoped_and_rewritten() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"
"#,
        );
        let mut plan = plan_from_config(&cfg);
        merge_sidecar_labels(
            &mut plan,
            "tools",
            BTreeMap::from([("fs".to_string(), short(&["label-fs", "/data"]))]),
        )
        .expect("merge succeeds");

        let fs = &plan.servers["fs"];
        assert_eq!(fs.placement, Placement::Sidecar("tools".to_string()));
        assert_eq!(fs.source, McpDeclarationSource::ImageLabel);
        // The spec was rewritten so its serialization shows the placement.
        assert_eq!(fs.spec.sidecar(), Some("tools"));
        assert_eq!(
            fs.spec.normalize().0,
            vec!["label-fs".to_string(), "/data".to_string()]
        );
    }

    #[test]
    fn config_overrides_sidecar_label_wholesale() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[mcp]
fs = ["config-fs"]
"#,
        );
        let mut plan = plan_from_config(&cfg);
        merge_sidecar_labels(
            &mut plan,
            "tools",
            BTreeMap::from([("fs".to_string(), short(&["label-fs"]))]),
        )
        .expect("config-overridden name is not a collision");

        // The config entry won: primary placement, config spec.
        let fs = &plan.servers["fs"];
        assert_eq!(fs.placement, Placement::Primary);
        assert_eq!(fs.source, McpDeclarationSource::ConfigToml);
        assert_eq!(fs.spec.normalize().0, vec!["config-fs".to_string()]);
    }

    #[test]
    fn cross_label_duplicate_is_an_error() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.a]
image = "img-a"

[sidecars.b]
image = "img-b"
"#,
        );
        let mut plan = plan_from_config(&cfg);
        merge_sidecar_labels(
            &mut plan,
            "a",
            BTreeMap::from([("fs".to_string(), short(&["fs-a"]))]),
        )
        .expect("first label wins a fresh name");
        let err = merge_sidecar_labels(
            &mut plan,
            "b",
            BTreeMap::from([("fs".to_string(), short(&["fs-b"]))]),
        )
        .expect_err("two labels claiming one name must collide");

        let message = err.to_string();
        assert!(
            message.contains("duplicate mcp server name \"fs\""),
            "{message}"
        );
        assert!(message.contains("sidecar \"a\""), "{message}");
        assert!(message.contains("sidecar \"b\""), "{message}");
    }

    #[test]
    fn primary_label_conflicts_with_sidecar_label() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"
"#,
        );
        let mut plan = plan_from_config(&cfg);
        merge_primary_labels(
            &mut plan,
            BTreeMap::from([("fs".to_string(), short(&["primary-fs"]))]),
        );
        let err = merge_sidecar_labels(
            &mut plan,
            "tools",
            BTreeMap::from([("fs".to_string(), short(&["sidecar-fs"]))]),
        )
        .expect_err("primary label vs sidecar label is a collision");
        assert!(err.to_string().contains("primary"), "{err}");
    }

    #[test]
    fn bootstrap_needed_for_exec_workspace_or_mounts() {
        let cfg = image_cfg(
            r#"
dockerfile = "D"
context    = "."

[sidecars.bare]
image = "img"

[sidecars.ws]
image     = "img"
workspace = "ro"

[sidecars.mounted]
image = "img"

[[sidecars.mounted.mounts]]
host-path      = "cache"
container-path = "/cache"

[sidecars.hosting]
image = "img"

[mcp]
fs = { command = ["mcp-fs"], sidecar = "hosting" }
"#,
        );
        let plan = plan_from_config(&cfg);

        assert!(!plan.sidecar_needs_bootstrap(&plan.sidecars["bare"]));
        assert!(plan.sidecar_needs_bootstrap(&plan.sidecars["ws"]));
        assert!(plan.sidecar_needs_bootstrap(&plan.sidecars["mounted"]));
        assert!(plan.sidecar_needs_bootstrap(&plan.sidecars["hosting"]));
    }
}
