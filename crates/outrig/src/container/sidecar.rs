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
use std::path::Path;

use crate::config::{
    Config, ContainerSecurity, ImageConfig, McpServerSpec, MountConfig, SidecarOnFailure,
    SidecarStart, SidecarView, SidecarWorkspaceAccess,
};
use crate::container::embedded::McpDeclarationSource;
use crate::error::{OutrigError, Result};

/// Which container hosts an MCP server. Anonymous sidecars are keyed by
/// their declaring server's name, so `Sidecar` covers both forms.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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

    /// The sidecar this placement names, or `None` for the primary. Lives
    /// here so callers ask one question instead of matching an enum that is
    /// `#[non_exhaustive]` to them.
    pub fn sidecar_name(&self) -> Option<&str> {
        match self {
            Self::Primary => None,
            Self::Sidecar(name) => Some(name),
        }
    }
}

/// One merged MCP server: its spec, where it was declared, and which
/// container it runs in.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct PlacedServer {
    pub spec: McpServerSpec,
    pub source: McpDeclarationSource,
    pub placement: Placement,
}

/// One sidecar container the session wants, named or anonymous. `image` is
/// unresolved -- an `[images.<name>]` config name or a raw podman ref.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SidecarPlan {
    pub name: String,
    pub image: String,
    /// Positional arguments for the image's ENTRYPOINT, meaningful only when
    /// this sidecar is an entrypoint host. Anonymous sidecars leave it empty:
    /// their arguments ride the declaring MCP entry's own `args`.
    pub args: Vec<String>,
    pub workspace: SidecarWorkspaceAccess,
    /// Whether the sidecar runs against the primary container's filesystem
    /// view. `Primary` is entrypoint-stdio only (validation enforces it) and
    /// mutually exclusive with a non-`None` `workspace`.
    pub view: SidecarView,
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
    /// The all-defaults plan an inline `image` key implies. `view` rides the
    /// declaring entry (the one-liner `image = ..., view = "primary"` form);
    /// everything else takes defaults.
    fn anonymous(server_name: &str, image: &str, view: SidecarView) -> Self {
        Self {
            name: server_name.to_string(),
            image: image.to_string(),
            args: Vec::new(),
            workspace: SidecarWorkspaceAccess::None,
            view,
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
#[non_exhaustive]
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

    /// Whether the sidecar needs the in-container user bootstrap: it hosts an
    /// exec-stdio server, takes a workspace view, or carries mounts. An
    /// entrypoint host never does -- bootstrap runs over `podman exec`, and
    /// there is no window for it between `podman create` and the `podman start
    /// --attach` that *is* the server. Such a container keeps the image's own
    /// `USER`, mounts or not -- except under [`SidecarView::Primary`], where
    /// the launcher drops the payload to the session's ids itself (see
    /// [`build_primary_view_argv`]) and reads the primary's `/etc/passwd`
    /// through the graft rather than needing one of its own.
    pub fn sidecar_needs_bootstrap(&self, sidecar: &SidecarPlan) -> bool {
        bootstrap_needed(
            self.entrypoint_server_in(sidecar).is_some(),
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

/// Prefix `elem` with `graft` iff it is an absolute path. Relative elements
/// pass through unchanged: they are not paths in the sidecar rootfs we can
/// relocate under the graft. A bare program name is one such element, and the
/// launcher resolves it against the sidecar image's `PATH` before the namespace
/// join (`enter/path_search.rs`) -- so `ENTRYPOINT ["node", ...]` runs the
/// sidecar's own `node`, not the primary's.
fn graft_prefix(elem: &str, graft: &str) -> String {
    if elem.starts_with('/') {
        format!("{graft}{elem}")
    } else {
        elem.to_string()
    }
}

/// Build the trailing argv a `view = "primary"` sidecar hands `outrig-enter`:
/// the launcher flags, `--`, then the payload command.
///
/// The payload is the sidecar image's ENTRYPOINT followed by either the
/// config-supplied `config_args` when present, or the image's CMD otherwise --
/// mirroring OCI's "args replace CMD". Two prefixing rules apply on top:
///
/// - Image-declared elements are graft-prefixed; they name files in the
///   sidecar's *own* rootfs, which lands under `graft` after the setns.
///   Config-declared `config_args` are passed bare: they name paths in the
///   *primary's* view.
/// - The payload's **program** -- its first element -- is always bare, even
///   though it is image-declared. The launcher opens it before the setns,
///   while the sidecar's own rootfs is still at `/`, and applies the graft
///   itself when handing the path to the loader. Prefixing it here would make
///   the launcher look for `<graft><graft>/...`. A program named without a
///   `/` is resolved along the sidecar image's `PATH` there too, so an
///   `ENTRYPOINT ["node", ...]` image needs no rewriting.
///
/// Relative elements pass through unprefixed either way -- they are not files
/// in the sidecar rootfs we can relocate.
///
/// `ids` is the `(uid, gid)` the payload runs as: the launcher holds
/// `CAP_SYS_ADMIN`/`CAP_SYS_PTRACE` only until the graft is in place, then
/// becomes these ids -- so what the server may do and what it may own match an
/// exec-stdio server's. `None` omits both flags, leaving the payload as
/// whatever the image's `USER` says; every OutRig-launched sidecar passes the
/// session's ids, and the option exists because the launcher is a standalone
/// binary with a documented argv contract, not only an OutRig internal.
pub fn build_primary_view_argv(
    entrypoint: &[String],
    cmd: &[String],
    config_args: &[String],
    graft: &str,
    cwd: &str,
    ns_file: &str,
    ids: Option<(u32, u32)>,
) -> Vec<String> {
    let mut argv = vec![
        "--ns-file".to_string(),
        ns_file.to_string(),
        "--graft".to_string(),
        graft.to_string(),
        "--cwd".to_string(),
        cwd.to_string(),
    ];
    if let Some((uid, gid)) = ids {
        argv.extend([
            "--uid".to_string(),
            uid.to_string(),
            "--gid".to_string(),
            gid.to_string(),
        ]);
    }
    argv.push("--".to_string());
    // Built in one pass rather than prefixing and then stripping back: a
    // config arg is legitimately allowed to name a path under the graft point
    // in the primary's view, and stripping would silently rewrite it.
    let (image_tail, bare_tail): (&[String], &[String]) = if config_args.is_empty() {
        (cmd, &[])
    } else {
        (&[], config_args)
    };
    let mut image_declared = entrypoint.iter().chain(image_tail);
    argv.extend(image_declared.next().cloned());
    argv.extend(image_declared.map(|e| graft_prefix(e, graft)));
    argv.extend(bare_tail.iter().cloned());
    argv
}

/// The trailing argv an entrypoint-stdio sidecar's container is created with,
/// for either filesystem view.
///
/// With [`SidecarView::None`] the image's ENTRYPOINT runs directly and `args`
/// are its positional arguments. With [`SidecarView::Primary`] the container's
/// entrypoint is the `outrig-enter` launcher instead, so the real payload has
/// to be reconstructed from the image's own ENTRYPOINT/CMD and handed over
/// behind the launcher's flags -- which is what [`build_primary_view_argv`]
/// does, with the `PRIMARY_VIEW_*` bind targets filled in here.
///
/// Both the CLI's session setup and the library facade's `add_sidecar` build
/// their `podman create` arguments through this one function, so a session
/// launched from a `Config` and one launched from a hand-built spec cannot
/// drift.
///
/// `container_workspace` becomes the payload's working directory. A session
/// without a workspace passes an empty path and lands on `/` -- the only
/// directory the primary's view is guaranteed to have. `ids` is forwarded to
/// [`build_primary_view_argv`]; without a view there is no launcher to hand it
/// to, and podman decides the user from the image.
pub fn entrypoint_create_args(
    view: SidecarView,
    image_entrypoint: &[String],
    image_cmd: &[String],
    args: &[String],
    container_workspace: &Path,
    ids: Option<(u32, u32)>,
) -> Vec<String> {
    match view {
        SidecarView::Primary => {
            let cwd = container_workspace.to_string_lossy();
            build_primary_view_argv(
                image_entrypoint,
                image_cmd,
                args,
                super::PRIMARY_VIEW_GRAFT,
                if cwd.is_empty() { "/" } else { &cwd },
                &format!(
                    "{}/{}",
                    super::PRIMARY_VIEW_NS_MOUNT,
                    super::PRIMARY_VIEW_NS_FILE
                ),
                ids,
            )
        }
        // Spelled out rather than a catch-all: `#[non_exhaustive]` does not
        // suppress exhaustiveness inside the defining crate, so a third view
        // mode is a compile error here rather than silently meaning "no view",
        // which would run the sidecar's own ENTRYPOINT instead of the launcher.
        SidecarView::None => args.to_vec(),
    }
}

/// Whether a sidecar needs the in-container user bootstrap: it hosts at least
/// one exec-stdio server (exec needs `--user` and `HOME`), sees the workspace,
/// or declares mounts.
///
/// An entrypoint host never does, whatever else it declares -- bootstrap runs
/// over `podman exec`, and there is no window for it between `podman create`
/// and the `podman start --attach` that *is* the server, so such a container
/// keeps the image's own `USER`. That short-circuit lives here rather than in
/// each caller: the config-plan path and `Outrig::add_sidecar`'s `SidecarSpec`
/// path both host entrypoint servers, and the rule is the same for both.
///
/// A [`SidecarView::Primary`] host is the exception to the `USER` half, not to
/// the short-circuit: it still cannot be bootstrapped, but it does not need to
/// be. [`build_primary_view_argv`] hands the launcher the session's ids, and
/// the graft puts the primary's already-bootstrapped `/etc/passwd` at `/`.
pub(crate) fn bootstrap_needed(
    is_entrypoint_host: bool,
    hosts_exec_server: bool,
    workspace: SidecarWorkspaceAccess,
    has_mounts: bool,
) -> bool {
    !is_entrypoint_host
        && (hosts_exec_server || workspace != SidecarWorkspaceAccess::None || has_mounts)
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
                view: sidecar.view,
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
            plan.sidecars.insert(
                name.clone(),
                SidecarPlan::anonymous(name, image, spec.view()),
            );
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
                    view: crate::config::SidecarView::None,
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
        assert!(!bootstrap_needed(false, false, NoWs, false));
        assert!(bootstrap_needed(false, true, NoWs, false));
        assert!(bootstrap_needed(false, false, Ro, false));
        assert!(bootstrap_needed(false, false, NoWs, true));
        // An entrypoint host is exempt on every axis at once.
        assert!(!bootstrap_needed(true, true, Ro, true));
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

    #[test]
    fn graft_prefix_only_touches_absolute_paths() {
        assert_eq!(
            graft_prefix("/usr/local/bin/node", "/mnt"),
            "/mnt/usr/local/bin/node"
        );
        // Relative elements (PATH-resolved) are left bare.
        assert_eq!(graft_prefix("node", "/mnt"), "node");
        assert_eq!(graft_prefix("/", "/mnt"), "/mnt/");
    }

    #[test]
    fn primary_view_argv_grafts_entrypoint_passes_config_args_bare() {
        // The task's worked example: image ENTRYPOINT grafted, config `args`
        // (a target path) passed bare -- except the program itself, which the
        // launcher opens pre-setns and grafts on its own.
        let argv = build_primary_view_argv(
            &[
                "/usr/local/bin/node".to_string(),
                "/app/dist/index.js".to_string(),
            ],
            &[], // no CMD
            &["/workspace".to_string()],
            "/mnt",
            "/workspace",
            "/target-ns/mnt",
            None,
        );
        assert_eq!(
            argv,
            [
                "--ns-file",
                "/target-ns/mnt",
                "--graft",
                "/mnt",
                "--cwd",
                "/workspace",
                "--",
                // Bare: `outrig-enter` opens this before the setns, while the
                // sidecar's own rootfs is still at `/`, then re-prefixes it
                // with the graft when handing it to the loader.
                "/usr/local/bin/node",
                "/mnt/app/dist/index.js",
                "/workspace",
            ]
        );
    }

    #[test]
    fn primary_view_argv_grafts_cmd_when_no_config_args() {
        // No config args -> the image CMD is used, graft-prefixed (OCI: `args`
        // replace CMD).
        let argv = build_primary_view_argv(
            &["/bin/server".to_string()],
            &["/default/dir".to_string()],
            &[],
            "/mnt",
            "/",
            "/target-ns/mnt",
            None,
        );
        assert_eq!(
            argv,
            [
                "--ns-file",
                "/target-ns/mnt",
                "--graft",
                "/mnt",
                "--cwd",
                "/",
                "--",
                "/bin/server",
                "/mnt/default/dir",
            ]
        );
    }

    /// When the image declares no ENTRYPOINT, CMD supplies the program, so the
    /// bare-program rule has to apply to the CMD element instead.
    #[test]
    fn primary_view_argv_leaves_a_cmd_supplied_program_bare() {
        let argv = build_primary_view_argv(
            &[],
            &["/bin/server".to_string(), "/default/dir".to_string()],
            &[],
            "/mnt",
            "/",
            "/target-ns/mnt",
            None,
        );
        assert_eq!(&argv[7..], ["/bin/server", "/mnt/default/dir"]);
    }

    /// A config arg may legitimately name a path that starts with the graft
    /// point -- `/mnt` is an ordinary directory in the primary's view. The
    /// bare-program rule must not rewrite it, which a "prefix everything then
    /// strip the program back" formulation would.
    #[test]
    fn primary_view_argv_does_not_rewrite_a_config_arg_under_the_graft_point() {
        let argv = build_primary_view_argv(
            &["/bin/server".to_string()],
            &[],
            &["/mnt/data".to_string()],
            "/mnt",
            "/",
            "/target-ns/mnt",
            None,
        );
        assert_eq!(&argv[7..], ["/bin/server", "/mnt/data"]);

        // Same, with the config arg in the program slot (no ENTRYPOINT).
        let argv = build_primary_view_argv(
            &[],
            &[],
            &["/mnt/data".to_string()],
            "/mnt",
            "/",
            "/target-ns/mnt",
            None,
        );
        assert_eq!(&argv[7..], ["/mnt/data"]);
    }

    /// The ids the payload drops to ride in the flag block, ahead of `--`, and
    /// change nothing else. Omitting them reproduces the argv exactly as it was
    /// before the launcher could drop at all -- the property that keeps the
    /// launcher independently runnable against its documented contract.
    #[test]
    fn primary_view_argv_emits_the_drop_flags_ahead_of_the_separator() {
        let argv = |ids| {
            build_primary_view_argv(
                &["/usr/local/bin/node".to_string()],
                &[],
                &["/workspace".to_string()],
                "/mnt",
                "/workspace",
                "/target-ns/mnt",
                ids,
            )
        };
        assert_eq!(
            argv(Some((1000, 1001))),
            [
                "--ns-file",
                "/target-ns/mnt",
                "--graft",
                "/mnt",
                "--cwd",
                "/workspace",
                "--uid",
                "1000",
                "--gid",
                "1001",
                "--",
                "/usr/local/bin/node",
                "/workspace",
            ]
        );
        assert_eq!(
            argv(None),
            [
                "--ns-file",
                "/target-ns/mnt",
                "--graft",
                "/mnt",
                "--cwd",
                "/workspace",
                "--",
                "/usr/local/bin/node",
                "/workspace",
            ]
        );
    }

    #[test]
    fn entrypoint_create_args_passes_args_through_without_a_view() {
        // No view: the image's own ENTRYPOINT runs, so its ENTRYPOINT/CMD are
        // podman's business and only the positional arguments are ours -- as
        // are the ids, which podman decides from the image's `USER`.
        assert_eq!(
            entrypoint_create_args(
                SidecarView::None,
                &["/bin/server".to_string()],
                &["/default/dir".to_string()],
                &["/workspace".to_string()],
                Path::new("/workspace"),
                Some((1000, 1001)),
            ),
            ["/workspace"]
        );
    }

    /// The config path and the library path build their `podman create`
    /// arguments through this one function, so equivalent inputs cannot
    /// produce different argv. Pinned against the constants rather than
    /// against `build_primary_view_argv`'s hand-passed strings, which is the
    /// duplication this function exists to remove.
    #[test]
    fn entrypoint_create_args_fills_in_the_primary_view_bind_targets() {
        let argv = entrypoint_create_args(
            SidecarView::Primary,
            &["/usr/local/bin/node".to_string()],
            &[],
            &["/workspace".to_string()],
            Path::new("/workspace"),
            Some((1000, 1001)),
        );
        assert_eq!(
            argv,
            build_primary_view_argv(
                &["/usr/local/bin/node".to_string()],
                &[],
                &["/workspace".to_string()],
                super::super::PRIMARY_VIEW_GRAFT,
                "/workspace",
                &format!(
                    "{}/{}",
                    super::super::PRIMARY_VIEW_NS_MOUNT,
                    super::super::PRIMARY_VIEW_NS_FILE
                ),
                Some((1000, 1001)),
            )
        );
        assert_eq!(argv[0], "--ns-file");
        assert_eq!(argv[1], "/target-ns/mnt");
        assert_eq!(argv[3], "/mnt");
        assert_eq!(&argv[6..10], ["--uid", "1000", "--gid", "1001"]);
    }

    /// A session with no workspace has an empty container path. `--cwd ""`
    /// would make the launcher `chdir("")` and die, so the fallback lives in
    /// the function that owns the flag rather than in whichever caller happens
    /// to be able to produce it.
    #[test]
    fn entrypoint_create_args_falls_back_to_root_without_a_workspace() {
        let argv = entrypoint_create_args(
            SidecarView::Primary,
            &["/bin/server".to_string()],
            &[],
            &[],
            Path::new(""),
            Some((1000, 1001)),
        );
        assert_eq!(argv[4], "--cwd");
        assert_eq!(argv[5], "/");
    }
}
