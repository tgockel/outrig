// Symlink-free path resolution against the rootfs currently mounted at `/`.
//
// `setns(CLONE_NEWNS)` moves this process's *root directory*, not only its
// mounts. So once the launcher has joined the primary's namespace, an absolute
// symlink found under the graft no longer resolves inside the graft -- the
// kernel restarts resolution at the primary's `/` and hands back one of *its*
// files. Debian's `/lib64/ld-linux-x86-64.so.2` is such a link; Ubuntu's is
// relative. That one difference is enough to exec the primary's dynamic loader
// against the sidecar's libc, which segfaults before either can say why.
//
// Resolving here, ahead of the setns, is what makes a path mean the same thing
// in both rootfs views. It settles the paths the launcher hands over, and only
// those: what the loader goes on to find for itself -- a `DT_NEEDED` library
// that is an absolute symlink, musl's `/etc/ld-musl-<arch>.path` -- escapes by
// the identical mechanism and is not fixed here.
//
// The one file in this directory that does I/O -- resolution *is* a sequence of
// `readlink`s. Like `elf.rs` and `path_search.rs` it is the single source of
// truth: `launcher.rs` pulls it in with `include!`, so its header must be plain
// `//` comments (inner `//!` docs are illegal mid-file) and it declares no
// `use` at all, which is how it stays free of collisions with that file's. The
// `outrig` crate also compiles it as `#[cfg(test)] mod canon` to run these
// tests on the host.

/// Symlink hops allowed before giving up, as the kernel's `SYMLOOP_MAX` has it.
const SYMLOOP_MAX: usize = 40;

const ENOENT: i32 = 2;
const EINVAL: i32 = 22;
const ELOOP: i32 = 40;

/// `bytes` as a path, without importing `OsStrExt`: `launcher.rs` imports it
/// already, and under `include!` a second import of the same trait is a
/// collision.
fn as_path(bytes: &[u8]) -> &std::path::Path {
    std::path::Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(bytes))
}

/// `path` split into components, innermost last -- a stack to `pop` from, so a
/// link's own components can be pushed in front of whatever is left to walk.
fn pending_components(path: &[u8]) -> Vec<Vec<u8>> {
    path.split(|&b| b == b'/')
        .rev()
        .map(|c| c.to_vec())
        .collect()
}

/// Resolve every symlink in `path`, treating `root` as the filesystem root.
///
/// The launcher passes `b""`: the rootfs currently mounted at `/`, which is the
/// sidecar's own until the setns and the primary's after it. **This must run
/// before the setns.** `root` is a parameter so the host tests can model the
/// kernel faithfully -- an absolute link target restarts at the root, which is
/// the escape this function exists to prevent, and a test that could only use
/// the real `/` could not stage one.
///
/// The result is `root`-relative and absolute, with no `.`, `..`, empty
/// component or symlink left in it. A component that does not exist stands as
/// written rather than failing here: the caller opens the path afterwards and
/// reports that in its own vocabulary, and it is what lets a dangling link
/// still resolve to the name it points at.
///
/// Errors are raw errnos: `EINVAL` for a `path` that is not absolute (nothing
/// here can name a working directory, and a relative path could not be
/// graft-prefixed anyway), `ELOOP` past `SYMLOOP_MAX` hops, and whatever
/// `readlink` reported for anything else.
fn canonicalize_under_root(root: &[u8], path: &[u8]) -> Result<Vec<u8>, i32> {
    if !path.starts_with(b"/") {
        return Err(EINVAL);
    }

    let mut pending = pending_components(path);
    let mut out: Vec<u8> = Vec::with_capacity(path.len());
    let mut hops = 0usize;

    while let Some(name) = pending.pop() {
        match name.as_slice() {
            b"" | b"." => continue,
            // `out` is symlink-free by construction, so the parent of what it
            // names is the lexical one -- which is exactly why `..` is only
            // safe to fold *after* the links ahead of it are resolved.
            b".." => {
                if let Some(slash) = out.iter().rposition(|&b| b == b'/') {
                    out.truncate(slash);
                }
                continue;
            }
            _ => {}
        }

        let mut probe = Vec::with_capacity(root.len() + out.len() + 1 + name.len());
        probe.extend_from_slice(root);
        probe.extend_from_slice(&out);
        probe.push(b'/');
        probe.extend_from_slice(&name);

        match std::fs::read_link(as_path(&probe)) {
            Ok(target) => {
                hops += 1;
                if hops > SYMLOOP_MAX {
                    return Err(ELOOP);
                }
                let target = target.as_os_str().as_encoded_bytes();
                // The kernel's rule, and the whole point of this walk: an
                // absolute target starts over at the root, a relative one
                // continues from the directory holding the link.
                if target.starts_with(b"/") {
                    out.clear();
                }
                pending.extend(pending_components(target));
            }
            // Not a symlink, or nothing there at all -- either way the
            // component stands as written.
            Err(e) if matches!(e.raw_os_error(), Some(EINVAL | ENOENT)) => {
                out.push(b'/');
                out.extend_from_slice(&name);
            }
            Err(e) => return Err(e.raw_os_error().unwrap_or(EINVAL)),
        }
    }

    if out.is_empty() {
        out.push(b'/');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rootfs staged under a tempdir. Every path is written relative to it,
    /// so a link target spelled absolutely means "this root" -- the shape the
    /// launcher faces, where the sidecar's rootfs is `/` while it resolves.
    struct Root(tempfile::TempDir);

    impl Root {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("tempdir"))
        }

        /// `path` under this root, with its parent directories made -- every
        /// staging call needs both, and nothing here needs a bare directory:
        /// resolution treats a missing component exactly as it treats a real
        /// one, so only the symlinks decide the answer.
        fn parent_of(&self, path: &str) -> std::path::PathBuf {
            let p = self.0.path().join(path.trim_start_matches('/'));
            std::fs::create_dir_all(p.parent().expect("a path under the root")).expect("mkdir -p");
            p
        }

        fn file(&self, path: &str) -> &Self {
            std::fs::write(self.parent_of(path), b"").expect("write file");
            self
        }

        /// A symlink at `path` holding `target` verbatim -- absolute or not,
        /// exactly as an image ships it.
        fn link(&self, path: &str, target: &str) -> &Self {
            std::os::unix::fs::symlink(target, self.parent_of(path)).expect("symlink");
            self
        }

        fn canon(&self, path: &str) -> Result<String, i32> {
            let root = self.0.path().as_os_str().as_encoded_bytes();
            let out = canonicalize_under_root(root, path.as_bytes())?;
            Ok(String::from_utf8(out).expect("the staged paths are all UTF-8"))
        }
    }

    /// The reported failure, in Debian's exact shape: an absolute interpreter
    /// link, reached through a second, relative one.
    #[test]
    fn an_absolute_target_restarts_at_the_root() {
        let root = Root::new();
        root.file("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2")
            .link("/lib", "usr/lib")
            .link(
                "/lib64/ld-linux-x86-64.so.2",
                "/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            );

        assert_eq!(
            root.canon("/lib64/ld-linux-x86-64.so.2"),
            Ok("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2".to_string()),
        );
    }

    /// Ubuntu's shape: the same link written relatively, with a `..` in it that
    /// has to fold against the link's own directory rather than the caller's.
    #[test]
    fn a_relative_target_resolves_against_the_links_own_directory() {
        let root = Root::new();
        root.file("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2")
            .link("/lib", "usr/lib")
            .link(
                "/lib64/ld-linux-x86-64.so.2",
                "../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2",
            );

        assert_eq!(
            root.canon("/lib64/ld-linux-x86-64.so.2"),
            Ok("/usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2".to_string()),
        );
    }

    #[test]
    fn a_chain_of_links_is_followed_to_the_end() {
        let root = Root::new();
        root.file("/opt/node/bin/node")
            .link("/usr/local/bin/node", "/usr/bin/node")
            .link("/usr/bin/node", "../../opt/node/bin/node");

        assert_eq!(
            root.canon("/usr/local/bin/node"),
            Ok("/opt/node/bin/node".to_string()),
        );
    }

    #[test]
    fn a_link_in_a_non_final_component_is_resolved_too() {
        let root = Root::new();
        root.file("/opt/server/dist/index.js")
            .link("/app", "/opt/server");

        assert_eq!(
            root.canon("/app/dist/index.js"),
            Ok("/opt/server/dist/index.js".to_string()),
        );
    }

    /// A link pointing at nothing still names something; the caller's `open`
    /// is what reports that it is not there.
    #[test]
    fn a_dangling_link_yields_the_target_it_names() {
        let root = Root::new();
        root.link("/lib64/ld.so", "/usr/lib/ld.so");

        assert_eq!(root.canon("/lib64/ld.so"), Ok("/usr/lib/ld.so".to_string()));
    }

    #[test]
    fn a_missing_component_is_kept_verbatim() {
        let root = Root::new();

        assert_eq!(
            root.canon("/nowhere/at/all"),
            Ok("/nowhere/at/all".to_string()),
        );
    }

    #[test]
    fn a_self_referential_link_is_eloop() {
        let root = Root::new();
        root.link("/lib/loop", "/lib/loop");

        assert_eq!(root.canon("/lib/loop"), Err(ELOOP));
    }

    /// One hop past the kernel's budget. The chain is walked, not detected as a
    /// cycle, so this is the bound rather than the loop check doing the work.
    #[test]
    fn a_chain_longer_than_symloop_max_is_eloop() {
        let root = Root::new();
        root.file("/end");
        for i in 0..=SYMLOOP_MAX {
            let target = if i == 0 {
                "/end".to_string()
            } else {
                format!("/hop{}", i - 1)
            };
            root.link(&format!("/hop{i}"), &target);
        }

        assert_eq!(
            root.canon(&format!("/hop{}", SYMLOOP_MAX - 1)),
            Ok("/end".to_string())
        );
        assert_eq!(root.canon(&format!("/hop{SYMLOOP_MAX}")), Err(ELOOP));
    }

    #[test]
    fn a_link_free_path_is_only_normalized() {
        let root = Root::new();
        assert_eq!(root.canon("//usr/./lib/"), Ok("/usr/lib".to_string()));
        assert_eq!(root.canon("/usr/lib/../lib"), Ok("/usr/lib".to_string()));
        assert_eq!(root.canon("/"), Ok("/".to_string()));
    }

    /// `..` at the root is the root, as it is for the kernel -- there is no
    /// component to pop and nowhere above it to reach.
    #[test]
    fn dot_dot_cannot_climb_past_the_root() {
        let root = Root::new();
        assert_eq!(root.canon("/../.."), Ok("/".to_string()));
        assert_eq!(root.canon("/../../usr"), Ok("/usr".to_string()));
    }

    #[test]
    fn a_relative_path_is_refused() {
        let root = Root::new();

        assert_eq!(root.canon("./server"), Err(EINVAL));
        assert_eq!(root.canon("server"), Err(EINVAL));
        assert_eq!(root.canon(""), Err(EINVAL));
    }
}
