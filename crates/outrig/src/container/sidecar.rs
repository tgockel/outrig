//! Session MCP placement planning.
//!
//! Turns an `[images.<name>]` block into a [`SessionMcpPlan`]: which sidecar
//! containers the session wants, and which container hosts each MCP server.
//! The plan is pure data -- image resolution, container starts, and MCP
//! connections happen in the caller (`session_setup` in the CLI) -- so the
//! merge and collision rules here are unit-testable without podman.
//!
//! Sidecars are declared once at the top level (`[sidecars.<sc>]`) and shared
//! across image-configs, so the plan holds the subset one image-config's
//! `[mcp]` entries name, not every block in the config.
//!
//! Merge order is deterministic: config entries first (they win wholesale,
//! placement included), then the primary image's `org.outrig.mcp` label, then
//! each named sidecar's label in name order. A name two labels both declare,
//! with no config override, is a hard error -- the per-session server
//! namespace is flat.

use std::collections::BTreeMap;

use crate::config::{
    Config, ContainerSecurity, ImageConfig, McpServerSpec, MountConfig, SidecarOnFailure,
    SidecarStart, SidecarWorkspaceAccess,
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
    /// Positional arguments for the image's ENTRYPOINT, meaningful only when
    /// this sidecar is an entrypoint host. Anonymous sidecars leave it empty:
    /// their arguments ride the declaring MCP entry's own `args`.
    pub args: Vec<String>,
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
            args: Vec::new(),
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

    /// Whether the sidecar needs the in-container user bootstrap; see
    /// [`bootstrap_needed`]. An entrypoint host never does: bootstrap runs
    /// over `podman exec`, and there is no window for it between `podman
    /// create` and the `podman start --attach` that *is* the server. Such a
    /// container keeps the image's own `USER`, mounts or not.
    pub fn sidecar_needs_bootstrap(&self, sidecar: &SidecarPlan) -> bool {
        if self.entrypoint_server_in(sidecar).is_some() {
            return false;
        }
        bootstrap_needed(
            self.servers_in(&sidecar.name)
                .any(|(_, placed)| placed.spec.has_command()),
            sidecar.workspace,
            !sidecar.mounts.is_empty(),
        )
    }

    /// The single entrypoint-stdio server this sidecar hosts: an entry with
    /// no `command`, so the image's ENTRYPOINT is the server and container
    /// lifetime equals server lifetime. Covers both the inline `image` form
    /// (anonymous) and a named block whose one entry omits `command`. `None`
    /// for exec-stdio hosts.
    ///
    /// The first match is the only one, kept true by two guards together:
    /// validation rejects a config that places a second server in an
    /// entrypoint host, and [`Self::sidecar_honors_labels`] keeps a label from
    /// adding one behind validation's back.
    pub fn entrypoint_server_in(&self, sidecar: &SidecarPlan) -> Option<(&String, &PlacedServer)> {
        self.servers_in(&sidecar.name)
            .find(|(_, placed)| placed.spec.is_entrypoint_stdio())
    }

    /// Whether this sidecar's image `org.outrig.mcp` label participates in
    /// the merge. Two kinds of sidecar run exactly one known server and so
    /// ignore it: anonymous ones (the inline `image` key declares the single
    /// server) and entrypoint hosts (the container process *is* the server).
    /// The one predicate behind both the Phase-A label read and the Phase-B
    /// merge, so an image is never inspected for a label nothing consumes.
    pub fn sidecar_honors_labels(&self, sidecar: &SidecarPlan) -> bool {
        !sidecar.anonymous && self.entrypoint_server_in(sidecar).is_none()
    }
}

/// The arguments an entrypoint host's container process receives: the
/// declaring MCP entry's `args`, else the sidecar block's. Validation rejects
/// setting both, so this is a choice, not a merge. Takes the spec directly --
/// every caller has already matched [`SessionMcpPlan::entrypoint_server_in`]
/// to know it is dealing with an entrypoint host.
pub fn entrypoint_args<'a>(spec: &'a McpServerSpec, sidecar: &'a SidecarPlan) -> &'a [String] {
    if spec.args().is_empty() {
        &sidecar.args
    } else {
        spec.args()
    }
}

/// Whether an *exec-stdio* sidecar needs the in-container user bootstrap: it
/// hosts at least one exec-stdio server (exec needs `--user` and `HOME`), sees
/// the workspace, or declares mounts. Shared by the config-plan path and
/// `Outrig::add_sidecar`'s `SidecarSpec` path, which cannot declare entrypoint
/// hosts at all. [`SessionMcpPlan::sidecar_needs_bootstrap`] short-circuits
/// ahead of this for entrypoint hosts, which never bootstrap.
pub fn bootstrap_needed(
    hosts_exec_server: bool,
    workspace: SidecarWorkspaceAccess,
    has_mounts: bool,
) -> bool {
    hosts_exec_server || workspace != SidecarWorkspaceAccess::None || has_mounts
}

/// Build the config-declared half of the plan: the `[sidecars.<sc>]` blocks
/// this image-config's `[mcp]` entries name, anonymous sidecars from inline
/// `image` keys, and every entry with its placement. Assumes the config has
/// passed validation, which is what guarantees each named block resolves.
///
/// Instantiation follows reference: a declared block no `[mcp]` entry names is
/// not in the plan and never starts. Blocks are top-level and shared, so most
/// of them belong to some *other* image-config.
pub fn plan_from_config(cfg: &Config, image_cfg: &ImageConfig) -> SessionMcpPlan {
    let mut plan = SessionMcpPlan::default();

    let referenced = image_cfg.mcp.values().filter_map(|spec| spec.sidecar());
    for sc in referenced {
        let Some(sidecar) = cfg.sidecars.get(sc) else {
            continue;
        };
        plan.sidecars.insert(
            sc.to_string(),
            SidecarPlan {
                name: sc.to_string(),
                image: sidecar.image.clone(),
                args: sidecar.args.clone(),
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
/// flat. Never called for entrypoint hosts (anonymous ones, and named blocks
/// whose entry omits `command`); their labels are inert, because exactly the
/// one declaring server runs there.
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
                    // Labels are exec-stdio-only (`parse_mcp_table` rejects
                    // `args` alongside the placement keys), so there is never
                    // anything to carry across.
                    args: Vec::new(),
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

    /// Parse a whole config and plan its `x` image-config, the way
    /// production does. Sidecar blocks are top-level and shared, so a plan
    /// only holds the ones `[images.x.mcp]` names.
    fn plan_of(toml_src: &str) -> SessionMcpPlan {
        let cfg: Config = toml::from_str(toml_src).expect("config parses");
        plan_from_config(&cfg, &cfg.images["x"])
    }

    fn short(cmd: &[&str]) -> McpServerSpec {
        McpServerSpec::Short(cmd.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn plan_places_servers_by_config_keys() {
        let plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[images.x.mcp]
local = ["mcp-local"]
fs    = { command = ["mcp-fs"], sidecar = "tools" }
grep  = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
"#,
        );

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
        let mut plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[images.x.mcp]
fs = { command = ["config-fs"], sidecar = "tools" }
"#,
        );
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
        let mut plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"
"#,
        );
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
        let mut plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[images.x.mcp]
fs = ["config-fs"]
"#,
        );
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
        let mut plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.a]
image = "img-a"

[sidecars.b]
image = "img-b"
"#,
        );
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
        let mut plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"
"#,
        );
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
        let plan = plan_of(
            r#"
[images.x]
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

[images.x.mcp]
a     = { command = ["mcp-a"], sidecar = "bare" }
b     = { command = ["mcp-b"], sidecar = "ws" }
c     = { command = ["mcp-c"], sidecar = "mounted" }
fs    = { command = ["mcp-fs"], sidecar = "hosting" }
fetch = { image = "ghcr.io/example/mcp-fetch:2" }
"#,
        );

        // Every sidecar a config can reach hosts a server, so hosting an
        // exec-stdio one is on its own enough -- workspace and mounts only
        // ever add to it.
        for name in ["bare", "ws", "mounted", "hosting"] {
            assert!(
                plan.sidecar_needs_bootstrap(&plan.sidecars[name]),
                "{name} hosts an exec-stdio server"
            );
        }
        // An entrypoint-stdio sidecar keeps the image's own USER untouched.
        assert!(!plan.sidecar_needs_bootstrap(&plan.sidecars["fetch"]));
    }

    /// The workspace and mounts axes in isolation. Only `Outrig::add_sidecar`
    /// reaches them without an exec-stdio server -- a config-declared sidecar
    /// is in the plan because some `[mcp]` entry named it.
    #[test]
    fn bootstrap_needed_axes() {
        use SidecarWorkspaceAccess::{None as NoWs, Ro};
        assert!(!bootstrap_needed(false, NoWs, false));
        assert!(bootstrap_needed(true, NoWs, false));
        assert!(bootstrap_needed(false, Ro, false));
        assert!(bootstrap_needed(false, NoWs, true));
    }

    #[test]
    fn entrypoint_server_found_in_any_no_command_sidecar() {
        let plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[sidecars.serve]
image = "mcp-serve"

[images.x.mcp]
fs    = { command = ["mcp-fs"], sidecar = "tools" }
srv   = { sidecar = "serve" }
grep  = { command = ["mcp-grep"], image = "ghcr.io/example/mcp-grep:1" }
fetch = { image = "ghcr.io/example/mcp-fetch:2" }
"#,
        );

        let (name, placed) = plan
            .entrypoint_server_in(&plan.sidecars["fetch"])
            .expect("no-command inline image is entrypoint-stdio");
        assert_eq!(name, "fetch");
        assert_eq!(placed.spec.image(), Some("ghcr.io/example/mcp-fetch:2"));

        // A named block whose entry omits `command` is an entrypoint host too.
        let (name, _) = plan
            .entrypoint_server_in(&plan.sidecars["serve"])
            .expect("no-command named sidecar is entrypoint-stdio");
        assert_eq!(name, "srv");

        // Anything carrying a command stays exec-stdio.
        assert!(plan.entrypoint_server_in(&plan.sidecars["grep"]).is_none());
        assert!(plan.entrypoint_server_in(&plan.sidecars["tools"]).is_none());
    }

    /// Only exec-stdio hosts read their image's `org.outrig.mcp`: the other
    /// two kinds run exactly one already-known server.
    #[test]
    fn only_exec_stdio_named_sidecars_honor_labels() {
        let plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.tools]
image = "mcp-tools"

[sidecars.serve]
image = "mcp-serve"

[images.x.mcp]
fs    = { command = ["mcp-fs"], sidecar = "tools" }
srv   = { sidecar = "serve" }
fetch = { image = "ghcr.io/example/mcp-fetch:2" }
"#,
        );

        assert!(plan.sidecar_honors_labels(&plan.sidecars["tools"]));
        assert!(!plan.sidecar_honors_labels(&plan.sidecars["serve"]));
        assert!(!plan.sidecar_honors_labels(&plan.sidecars["fetch"]));
    }

    /// An entrypoint host never bootstraps: there is no exec window between
    /// `podman create` and the `podman start --attach` that is the server, so
    /// declaring a workspace does not conjure one.
    #[test]
    fn entrypoint_host_skips_bootstrap_even_with_a_workspace() {
        let plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.serve]
image     = "mcp-serve"
workspace = "ro"

[images.x.mcp]
srv = { sidecar = "serve" }
"#,
        );
        assert!(!plan.sidecar_needs_bootstrap(&plan.sidecars["serve"]));
    }

    /// `args` may be declared on the entry or on the block; validation
    /// rejects both, so the plan picks rather than merges.
    #[test]
    fn entrypoint_args_come_from_the_entry_else_the_block() {
        let plan = plan_of(
            r#"
[images.x]
dockerfile = "D"
context    = "."

[sidecars.blockside]
image = "mcp-serve"
args  = ["/from-block"]

[sidecars.entryside]
image = "mcp-serve"

[images.x.mcp]
a = { sidecar = "blockside" }
b = { sidecar = "entryside", args = ["/from-entry"] }
c = { image = "ghcr.io/example/mcp-fetch:2", args = ["/inline"] }
"#,
        );
        let args_for = |sc: &str| {
            let sidecar = &plan.sidecars[sc];
            let (_, placed) = plan
                .entrypoint_server_in(sidecar)
                .expect("sidecar is an entrypoint host");
            entrypoint_args(&placed.spec, sidecar).to_vec()
        };

        assert_eq!(args_for("blockside"), ["/from-block"]);
        assert_eq!(args_for("entryside"), ["/from-entry"]);
        assert_eq!(args_for("c"), ["/inline"]);
    }
}
