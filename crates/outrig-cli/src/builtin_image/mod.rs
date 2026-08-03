//! outrig's built-in default image-config.
//!
//! A session that names no image -- no `--image`, no `agents.<n>.image`, no
//! `default-image` -- used to be a hard error. This module supplies the last
//! rung instead, so `outrig run` works in a directory with no
//! `.agents/outrig/config.toml` at all.
//!
//! The shape lives in [`default.toml`](./default.toml) and is parsed by the
//! same public `Config::load_from_str` a user's file goes through, so the
//! built-in is ordinary config rather than a parallel construction path. Two
//! things TOML cannot carry are applied on top in [`shape`]: the shell image's
//! absolute cache path, and the launcher-absent degradation.
//!
//! This is CLI policy, not library behavior. It is injected into an
//! already-loaded [`Config`] rather than into `Config::load*`, which is
//! supported API of the `outrig` crate -- an embedder must get their file's
//! contents, not the CLI's product opinion.

use std::path::{Path, PathBuf};

use outrig::config::{Config, ImageConfig, SidecarView, SidecarWorkspaceAccess};
use outrig::container::enter;
use outrig::error::Result;

use crate::paths;

/// The image-config a fallen-through cascade resolves to.
pub(crate) const DEFAULT_IMAGE: &str = "outrig-default";
/// Image-config and sidecar block hosting the filesystem server.
pub(crate) const FS_IMAGE: &str = "outrig-default-fs";
/// Image-config and sidecar block hosting the shell server.
pub(crate) const SHELL_IMAGE: &str = "outrig-default-shell";

/// Reserved `[images.<name>]` names.
const RESERVED_IMAGES: [&str; 3] = [DEFAULT_IMAGE, FS_IMAGE, SHELL_IMAGE];
/// Reserved `[sidecars.<name>]` names. `DEFAULT_IMAGE` is not among them --
/// the built-in declares no sidecar by that name, so a user's would not clash.
const RESERVED_SIDECARS: [&str; 2] = [FS_IMAGE, SHELL_IMAGE];

const DEFAULT_TOML: &str = include_str!("default.toml");
const SHELL_DOCKERFILE: &str = include_str!("mcp-shell.dockerfile");

/// The MCP server name dropped when the launcher is unavailable. Matches the
/// key in `default.toml`'s `[images.outrig-default.mcp]`.
const SHELL_SERVER: &str = "shell";

/// Whether `name` is one outrig reserves for the built-in default.
pub(crate) fn is_reserved(name: &str) -> bool {
    RESERVED_IMAGES.contains(&name)
}

/// The banner marker for an image-config that came from the built-in default.
/// Shared by `run` and `mcp` so the two banners cannot word it differently.
pub(crate) fn banner_suffix(builtin_default: bool) -> &'static str {
    if builtin_default {
        " (built-in default)"
    } else {
        ""
    }
}

/// The outcome of [`inject`]. Notes are *held* rather than printed: a repo
/// that never falls through to the built-in should stay silent, and only the
/// caller knows whether the resolved name was ours.
pub(crate) struct Injection {
    /// The image-config name to resolve, or `None` when nothing usable ended
    /// up under [`DEFAULT_IMAGE`]. `None` happens when a user declares one of
    /// the reserved *sidecar* names without declaring the image: injection is
    /// vetoed, and there is no `[images.outrig-default]` to fall back to.
    /// Returning the name rather than assuming it is what keeps that case from
    /// surfacing as an error about a block the user never wrote.
    pub(crate) resolved: Option<&'static str>,
    /// The built-in itself is in play, as opposed to a user's block that
    /// happens to sit under the same name. Drives the banner marker and the
    /// `outrig__*` self-documentation tools.
    pub(crate) applied: bool,
    pub(crate) notes: Vec<String>,
}

/// Add the built-in default's image-configs and sidecars to `cfg`, unless the
/// user already declares one of the reserved names.
pub(crate) fn inject(cfg: &mut Config) -> Injection {
    if let Some(note) = shadow_note(cfg) {
        return Injection {
            // Their `[images.outrig-default]` is usable; a shadow from one of
            // the sidecar names leaves nothing to resolve.
            resolved: cfg
                .images
                .contains_key(DEFAULT_IMAGE)
                .then_some(DEFAULT_IMAGE),
            applied: false,
            notes: vec![note],
        };
    }

    let mut notes = Vec::new();
    let launcher = enter::is_available();
    if !launcher {
        notes.push(
            "warning: this build has no `outrig-enter` launcher, so the built-in default's \
             `fs` server falls back to a read-write bind mount of the workspace and its \
             `shell` server is unavailable. Install the <arch>-unknown-linux-musl target and \
             rebuild outrig -- see doc/usage/run.md#the-built-in-default-image."
                .to_string(),
        );
    }

    // Only materialize when the launcher can actually run it; without one the
    // shell is dropped either way, and writing a Dockerfile nothing builds is
    // pointless work in every session.
    let shell_dir = launcher.then(|| shell_dir_or_note(&mut notes)).flatten();

    // `or_insert`, not `extend`: the built-in is the *lowest*-precedence layer,
    // below both config files. `extend` would state the opposite and leave the
    // shadow veto above as the only thing correcting it.
    let built = shape(launcher, shell_dir.as_deref());
    for (name, image) in built.images {
        cfg.images.entry(name).or_insert(image);
    }
    for (name, sidecar) in built.sidecars {
        cfg.sidecars.entry(name).or_insert(sidecar);
    }

    Injection {
        resolved: Some(DEFAULT_IMAGE),
        applied: true,
        notes,
    }
}

/// The materialized shell build context, or `None` with a note appended. A
/// cache directory that cannot be written drops the shell server; it must not
/// make the session unstartable.
fn shell_dir_or_note(notes: &mut Vec<String>) -> Option<PathBuf> {
    let dir = paths::builtin_image_dir(SHELL_IMAGE);
    match materialize(&dir) {
        Ok(()) => Some(dir),
        Err(err) => {
            notes.push(format!(
                "warning: could not write the built-in shell image under {}: {err} -- the \
                 built-in default's `shell` server is unavailable",
                dir.display()
            ));
            None
        }
    }
}

/// Write the embedded Dockerfile into `dir`, skipping the write when the
/// bytes already match. Rewriting every session would be harmless for the
/// content hash -- `hash_tar_context` pins mtime and ownership -- but it is
/// pointless I/O on a path every run takes.
fn materialize(dir: &Path) -> Result<()> {
    let path = dir.join("Dockerfile");
    if std::fs::read_to_string(&path).is_ok_and(|current| current == SHELL_DOCKERFILE) {
        return Ok(());
    }
    paths::write_atomic(&path, SHELL_DOCKERFILE)
}

/// The built-in shape for one set of run-time facts. Pure, so both launcher
/// modes are testable on a host that does have the launcher.
///
/// `shell_dir` is the materialized build context, or `None` when the shell
/// server is unavailable -- no launcher to run it, or nowhere to write it.
fn shape(launcher: bool, shell_dir: Option<&Path>) -> Config {
    let mut cfg = Config::load_from_str(DEFAULT_TOML)
        .expect("built-in default config is a compile-time constant, and a test parses it");

    match shell_dir {
        // Set in Rust rather than substituted into the TOML: an absolute cache
        // path is not guaranteed to be TOML-safe (a backslash is a legal byte
        // in a Linux path and an escape in a TOML basic string).
        Some(dir) => {
            cfg.images.insert(
                SHELL_IMAGE.to_string(),
                ImageConfig::from_dockerfile(dir.join("Dockerfile"), dir),
            );
        }
        None => {
            cfg.sidecars.remove(SHELL_IMAGE);
            if let Some(primary) = cfg.images.get_mut(DEFAULT_IMAGE) {
                primary.mcp.remove(SHELL_SERVER);
            }
        }
    }

    // A bind mount is the honest degradation for `fs`: reading and writing the
    // workspace is the same operation either way. It is not honest for a
    // shell, which is why `shell` is dropped above rather than demoted.
    if !launcher && let Some(fs) = cfg.sidecars.get_mut(FS_IMAGE) {
        fs.view = SidecarView::None;
        fs.workspace = SidecarWorkspaceAccess::Rw;
    }

    cfg
}

/// A note naming the first reserved name the user already declares, if any.
///
/// Injection is all-or-nothing. Partial injection produces broken configs: a
/// user `[images.outrig-default]` alongside our `[sidecars.outrig-default-fs]`
/// leaves that block's `args` unreachable, which is a hard
/// `SidecarArgsWithoutEntrypoint` for every command in that repo.
fn shadow_note(cfg: &Config) -> Option<String> {
    let declared_in = |image: &ImageConfig| {
        image
            .config_source()
            .map(|source| format!(" (declared in {})", source.config_path().display()))
            .unwrap_or_default()
    };

    RESERVED_IMAGES
        .iter()
        .find_map(|name| {
            cfg.images.get(*name).map(|image| {
                format!(
                    "note: `[images.{name}]`{} shadows outrig's built-in default \
                     image-config; the built-in is not in play",
                    declared_in(image)
                )
            })
        })
        .or_else(|| {
            RESERVED_SIDECARS
                .iter()
                .find(|name| cfg.sidecars.contains_key(**name))
                .map(|name| {
                    format!(
                        "note: `[sidecars.{name}]` shadows outrig's built-in default \
                         image-config; the built-in is not in play"
                    )
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    use outrig::config::SidecarConfig;
    use tempfile::TempDir;

    /// A materialized build context, so the shape referencing it validates.
    fn shell_context() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        materialize(dir.path()).expect("materialize");
        dir
    }

    /// The guard that proves the built-in shape is legal config. It reaches
    /// every rule the shape could trip -- placement conflicts, unreachable
    /// sidecar args, an entrypoint-less `view`, an invalid build-image name,
    /// and a missing Dockerfile.
    #[test]
    fn both_shapes_validate() {
        let ctx = shell_context();
        let repo = TempDir::new().expect("tempdir");

        for (launcher, dir) in [(true, Some(ctx.path())), (false, None)] {
            shape(launcher, dir)
                .validate(Some(repo.path()))
                .unwrap_or_else(|e| panic!("shape(launcher={launcher}) must validate: {e}"));
        }
    }

    #[test]
    fn with_launcher_both_servers_run_against_the_primary_view() {
        let ctx = shell_context();
        let cfg = shape(true, Some(ctx.path()));

        let primary = &cfg.images[DEFAULT_IMAGE];
        assert_eq!(primary.mcp.len(), 2, "fs and shell");
        assert_eq!(primary.mcp["fs"].sidecar(), Some(FS_IMAGE));
        assert_eq!(primary.mcp[SHELL_SERVER].sidecar(), Some(SHELL_IMAGE));

        for name in [FS_IMAGE, SHELL_IMAGE] {
            assert_eq!(cfg.sidecars[name].view, SidecarView::Primary, "{name}");
            assert_eq!(
                cfg.sidecars[name].workspace,
                SidecarWorkspaceAccess::None,
                "{name} must not also bind the workspace"
            );
        }
        assert_eq!(cfg.sidecars[FS_IMAGE].args, ["/workspace"]);
        assert!(cfg.images.contains_key(SHELL_IMAGE));
    }

    #[test]
    fn without_launcher_fs_degrades_to_a_bind_mount_and_shell_is_dropped() {
        let cfg = shape(false, None);

        let fs = &cfg.sidecars[FS_IMAGE];
        assert_eq!(fs.view, SidecarView::None);
        assert_eq!(fs.workspace, SidecarWorkspaceAccess::Rw);
        assert_eq!(
            fs.args,
            ["/workspace"],
            "args name the same tree either way"
        );

        assert!(!cfg.sidecars.contains_key(SHELL_IMAGE));
        assert!(!cfg.images.contains_key(SHELL_IMAGE));
        assert!(!cfg.images[DEFAULT_IMAGE].mcp.contains_key(SHELL_SERVER));
    }

    /// The launcher is present but the cache directory was not writable.
    #[test]
    fn shell_is_dropped_without_a_context_even_when_the_launcher_is_present() {
        let cfg = shape(true, None);

        assert!(!cfg.images.contains_key(SHELL_IMAGE));
        assert!(!cfg.images[DEFAULT_IMAGE].mcp.contains_key(SHELL_SERVER));
        assert_eq!(
            cfg.sidecars[FS_IMAGE].view,
            SidecarView::Primary,
            "fs keeps the view; only the shell depended on the context"
        );
    }

    #[test]
    fn materialize_writes_once_and_is_idempotent() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("Dockerfile");

        materialize(dir.path()).expect("first");
        let first = std::fs::metadata(&path).expect("written").modified().ok();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SHELL_DOCKERFILE);

        materialize(dir.path()).expect("second");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("still there")
                .modified()
                .ok(),
            first,
            "matching bytes must not be rewritten"
        );
    }

    #[test]
    fn materialize_replaces_stale_content() {
        let dir = TempDir::new().expect("tempdir");
        paths::write_atomic(&dir.path().join("Dockerfile"), "FROM scratch\n").expect("stale");

        materialize(dir.path()).expect("refresh");

        let written = std::fs::read_to_string(dir.path().join("Dockerfile")).unwrap();
        assert_eq!(written, SHELL_DOCKERFILE);
    }

    /// Two ways this Dockerfile can rot silently. An unpinned `npm install -g`
    /// changes what the built-in runs between one session and the next; and
    /// the repo's own `mcp-shell` image is the only place this content is
    /// proved against a real session, so the two copies must agree on both the
    /// pin and the `ENTRYPOINT` the launcher requires be a real binary rather
    /// than a `#!` console script.
    #[test]
    fn the_embedded_dockerfile_is_pinned_and_matches_the_dogfooded_image() {
        // `fn` rather than closures: these tie the returned slice's lifetime to
        // the input, which a closure cannot express without annotation.
        fn pin(text: &str) -> &str {
            text.split_once("mcp-server-commands@")
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .expect("`mcp-server-commands` must be pinned with `@<version>`")
        }
        fn entrypoint_interpreter(text: &str) -> &str {
            text.split_once("ENTRYPOINT [")
                .and_then(|(_, rest)| rest.split('"').nth(1))
                .expect("an ENTRYPOINT naming the interpreter")
        }

        let dogfood = include_str!("../../../../.agents/outrig/images/mcp-shell/Dockerfile");
        let embedded = pin(SHELL_DOCKERFILE);
        assert!(
            embedded.starts_with(|c: char| c.is_ascii_digit()),
            "expected a version after `mcp-server-commands@`, got {embedded:?}"
        );
        assert_eq!(
            embedded,
            pin(dogfood),
            "pin drifted from the dogfooded image"
        );
        assert_eq!(
            entrypoint_interpreter(SHELL_DOCKERFILE),
            entrypoint_interpreter(dogfood),
            "ENTRYPOINT drifted from the dogfooded image"
        );
    }

    #[test]
    fn injection_is_skipped_when_a_reserved_image_name_is_taken() {
        for name in RESERVED_IMAGES {
            let mut cfg = Config::default();
            cfg.images
                .insert(name.to_string(), ImageConfig::from_image_name("mine:1"));

            let injection = inject(&mut cfg);

            assert!(!injection.applied, "{name}");
            assert_eq!(cfg.images.len(), 1, "{name}: nothing else was added");
            assert!(cfg.sidecars.is_empty(), "{name}: no sidecars were added");
            assert!(
                injection.notes[0].contains(name),
                "{name}: named in the note"
            );
        }
    }

    #[test]
    fn injection_is_skipped_when_a_reserved_sidecar_name_is_taken() {
        for name in RESERVED_SIDECARS {
            let mut cfg = Config::default();
            cfg.sidecars
                .insert(name.to_string(), SidecarConfig::new("mine:1"));

            let injection = inject(&mut cfg);

            assert!(!injection.applied, "{name}");
            assert!(cfg.images.is_empty(), "{name}: nothing was added");
            assert_eq!(cfg.sidecars.len(), 1, "{name}");
            assert!(
                injection.notes[0].contains(name),
                "{name}: named in the note"
            );
        }
    }

    #[test]
    fn reserved_names_are_exactly_the_ones_injected() {
        let ctx = shell_context();
        let cfg = shape(true, Some(ctx.path()));

        for name in cfg.images.keys() {
            assert!(is_reserved(name), "{name} is injected but not reserved");
        }
        for name in cfg.sidecars.keys() {
            assert!(
                RESERVED_SIDECARS.contains(&name.as_str()),
                "{name} is injected but not reserved"
            );
        }
    }
}
