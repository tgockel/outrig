//! The static CPython every session mounts, and where it lives on the host.
//!
//! `build.rs` fetches the pinned `python-build-standalone` release, verifies
//! it, and embeds it. A session's first start unpacks it under the user's cache
//! directory, and every session binds that tree read-only at
//! [`PAYLOAD_MOUNT`]. Nothing falls back to whatever `python3` the image
//! happens to carry, which would be a different interpreter with a different
//! library, or none at all.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use nix::sys::statvfs::{FsFlags, statvfs};

use crate::error::{IoPathExt, OutrigError, Result};

/// The directory OutRig claims inside a session's primary container. Nothing
/// the caller mounts may land at or under it.
pub(crate) const OUTRIG_ROOT: &str = "/outrig";

/// Where the payload is bound, read-only, inside the primary container.
pub(crate) const PAYLOAD_MOUNT: &str = "/outrig/python";

/// The pinned archive, as `build.rs` verified it; empty when that build could
/// not fetch it.
static ARCHIVE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/python.tar.zst"));

/// The archive's name less `.tar.zst`, which is also the directory it unpacks
/// to -- so a new pin unpacks beside an old one rather than over it.
const PAYLOAD: &str = env!("OUTRIG_PYTHON_PAYLOAD");

/// The payload's directory on the host, ready to bind at [`PAYLOAD_MOUNT`],
/// unpacked from the embedded archive the first time it is asked for.
///
/// Built for the *target* architecture. podman will run a foreign-arch image
/// under emulation without saying so, and this does not detect that; see
/// `plan/next/enter-arch-mismatch.md`, which the launcher shares.
pub(crate) async fn host_dir() -> Result<PathBuf> {
    if ARCHIVE.is_empty() {
        return Err(not_embedded());
    }
    let root = cache_root(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
        .ok_or_else(|| {
            OutrigError::Configuration(
                "cannot place the Python payload: neither an absolute XDG_CACHE_HOME nor HOME \
                 is set"
                    .to_string(),
            )
        })?;
    let dir = root.join("outrig/python").join(PAYLOAD);
    if dir.is_dir() {
        runnable_from(&dir)?;
        return Ok(dir);
    }
    // The blocking task, not this future, holds the lock through the unpack,
    // so a launch cancelled while it waits cannot let another start a second.
    let target = dir.clone();
    tokio::task::spawn_blocking(move || unpack_once(ARCHIVE, &target))
        .await
        .map_err(|e| OutrigError::Io(std::io::Error::other(e)))??;
    Ok(dir)
}

/// Refuse a caller-chosen container destination at or under [`OUTRIG_ROOT`].
/// A mount there would shadow the payload, or -- at `/outrig` itself -- make
/// the runtime create `python/` inside the caller's host directory to bind
/// over. Compared by components after resolving `.` and `..` as the runtime
/// will, so `/outrigger` passes and `/tmp/../outrig/python` does not.
pub(crate) fn reject_reserved(destination: &Path) -> Result<()> {
    if lexical(destination).starts_with(OUTRIG_ROOT) {
        return Err(OutrigError::Configuration(format!(
            "container path {} is under {OUTRIG_ROOT}, which OutRig reserves for the \
             Python payload it mounts at {PAYLOAD_MOUNT}",
            destination.display()
        )));
    }
    Ok(())
}

/// `path` with `.` and `..` resolved textually. A container destination names
/// a path in the container, so the host filesystem has nothing to say about
/// it; `..` at the root stays at the root.
fn lexical(path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => resolved.push(other),
        }
    }
    resolved
}

/// Refuse a payload directory on a filesystem mounted `noexec`. A bind mount
/// inherits the flag and rootless podman cannot clear it, so the container
/// would start and then fail to run its interpreter.
fn runnable_from(path: &Path) -> Result<()> {
    let stat = statvfs(path)
        .map_err(std::io::Error::from)
        .path_ctx("inspect the filesystem of", path)?;
    if stat.flags().contains(FsFlags::ST_NOEXEC) {
        return Err(OutrigError::Configuration(format!(
            "{} is on a filesystem mounted noexec, so the Python payload cannot run from it\n\
             help: point XDG_CACHE_HOME at a directory on a filesystem that allows execution",
            path.display()
        )));
    }
    Ok(())
}

/// The lock that serializes unpacking `dir`, beside it.
fn lock_path(dir: &Path) -> PathBuf {
    let name = dir.file_name().expect("the payload directory has a name");
    dir.with_file_name(format!(".{}.lock", name.to_string_lossy()))
}

/// Unpack `archive` to `dir` unless another thread or process has by the time
/// this one holds the lock. One unpack decodes through a 128 MiB window and
/// writes about 170 MB, so concurrent first launches must not each do it.
fn unpack_once(archive: &[u8], dir: &Path) -> Result<()> {
    let parent = dir.parent().expect("the payload directory has a parent");
    std::fs::create_dir_all(parent).path_ctx("create", parent)?;
    runnable_from(parent)?;
    let lock = lock_path(dir);
    let file = std::fs::File::create(&lock).path_ctx("create", &lock)?;
    let _held = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| std::io::Error::from(errno))
        .path_ctx("lock", &lock)?;
    if dir.is_dir() {
        return Ok(());
    }
    unpack(archive, dir)
}

/// What a build that degraded to an empty archive says at session start.
/// `build.rs` records why in `OUTRIG_PYTHON_UNAVAILABLE_REASON`.
fn not_embedded() -> OutrigError {
    let reason = option_env!("OUTRIG_PYTHON_UNAVAILABLE_REASON").unwrap_or("unknown");
    OutrigError::Configuration(format!(
        "this outrig was built without its Python payload: {reason}\n\
         help: rebuild with network access, or with OUTRIG_PYTHON_ARCHIVE naming a copy \
         of {PAYLOAD}.tar.zst"
    ))
}

/// The cache directory: `XDG_CACHE_HOME` when it is absolute, as the XDG spec
/// requires, otherwise `$HOME/.cache`. `build.rs`'s `download_cache` applies
/// the same rule.
fn cache_root(xdg_cache_home: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    xdg_cache_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
}

/// Unpack `archive`'s `python/install` tree to `dir`. It goes to a sibling
/// first and is renamed into place once complete, so `dir` either holds a
/// whole payload or does not exist. [`unpack_once`] keeps two from racing;
/// should one still lose a rename, it uses the winner's tree.
fn unpack(archive: &[u8], dir: &Path) -> Result<()> {
    let parent = dir.parent().expect("the payload directory has a parent");
    std::fs::create_dir_all(parent).path_ctx("create", parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".unpack-")
        .tempdir_in(parent)
        .path_ctx("create a directory in", parent)?;
    let at = stage.path();
    fn unpacking<T>(result: std::io::Result<T>, at: &Path) -> Result<T> {
        result.path_ctx("unpack Python into", at)
    }

    // No window limit, as in `build.rs`: these bytes matched the pinned digest
    // before they were embedded, and that archive needs 128 MiB.
    let decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(archive, u64::MAX)
        .map_err(std::io::Error::other);
    let mut tar = tar::Archive::new(unpacking(decoder, at)?);
    for entry in unpacking(tar.entries(), at)? {
        let mut entry = unpacking(entry, at)?;
        if !entry
            .path()
            .is_ok_and(|path| path.starts_with("python/install"))
        {
            continue;
        }
        // `unpack_in` refuses a member that would land outside the stage and
        // says so by returning `false`; a tree missing one is not a payload.
        if !unpacking(entry.unpack_in(at), at)? {
            return Err(OutrigError::Configuration(format!(
                "the embedded Python payload has a member outside its tree: {}",
                String::from_utf8_lossy(&entry.path_bytes())
            )));
        }
    }

    match std::fs::rename(stage.path().join("python/install"), dir) {
        Ok(()) => Ok(()),
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(e).path_ctx("move the Python payload to", dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// A tar.zst laid out as the release archives are: `python/install` beside
    /// `python/build`, no directory entries, and `bin/python3` a symlink.
    fn archive() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, bytes) in [
            ("python/PYTHON.json", &b"{}"[..]),
            ("python/build/Modules/Setup", b"build only"),
            ("python/install/bin/python3.13", b"interpreter"),
            ("python/install/lib/python3.13/os.py", b"library"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        tar.append_link(&mut link, "python/install/bin/python3", "python3.13")
            .unwrap();
        let tar = tar.into_inner().unwrap();
        ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest)
    }

    #[test]
    fn an_absolute_xdg_cache_home_wins() {
        assert_eq!(
            cache_root(os("/xdg"), os("/home/u")),
            Some(PathBuf::from("/xdg"))
        );
    }

    #[test]
    fn a_relative_or_empty_xdg_cache_home_falls_back_to_home() {
        for xdg in [os("relative/cache"), os(""), None] {
            assert_eq!(
                cache_root(xdg, os("/home/u")),
                Some(PathBuf::from("/home/u/.cache"))
            );
        }
    }

    #[test]
    fn no_usable_variable_is_no_cache_root() {
        assert_eq!(cache_root(os("relative"), None), None);
        assert_eq!(cache_root(None, os("")), None);
    }

    #[test]
    fn outrig_and_everything_under_it_is_reserved() {
        for path in [
            "/outrig",
            "/outrig/",
            "/outrig/python",
            "/outrig/work/deeper",
            // Aliases the runtime resolves to the same place.
            "/tmp/../outrig/python/bin",
            "/./outrig",
            "//outrig/python",
            "/../outrig",
        ] {
            let err = reject_reserved(Path::new(path)).unwrap_err().to_string();
            assert!(err.contains("reserves"), "{path}: {err}");
        }
        for path in [
            "/workspace",
            "/outrigger",
            "/opt/outrig",
            "/outrig-enter",
            "/outrig/../workspace",
        ] {
            reject_reserved(Path::new(path)).unwrap();
        }
    }

    /// `/proc` is mounted `noexec` on every Linux host; a fresh tempdir is not.
    #[test]
    fn a_noexec_filesystem_is_refused_before_anything_is_unpacked() {
        let err = runnable_from(Path::new("/proc")).unwrap_err().to_string();
        assert!(
            err.contains("/proc is on a filesystem mounted noexec"),
            "{err}"
        );
        assert!(err.contains("XDG_CACHE_HOME"), "{err}");
        runnable_from(tempfile::tempdir().unwrap().path()).unwrap();
    }

    /// A launch that waited on the lock finds the payload another one unpacked
    /// meanwhile, and does not unpack it again. The waiter is handed bytes that
    /// are not an archive, so an unpack it attempted anyway would be an error
    /// -- where a real one would lose the rename and pass unnoticed.
    #[test]
    fn an_unpack_that_waited_for_the_lock_uses_the_finished_tree() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join(PAYLOAD);
        let lock = std::fs::File::create(lock_path(&dir)).unwrap();
        let held = Flock::lock(lock, FlockArg::LockExclusive).unwrap();

        let waiter = {
            let dir = dir.clone();
            std::thread::spawn(move || unpack_once(b"not an archive", &dir))
        };
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("winner"), b"").unwrap();
        drop(held);

        waiter.join().unwrap().unwrap();
        assert!(dir.join("winner").exists());
    }

    #[test]
    fn only_the_install_tree_is_unpacked() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("python").join(PAYLOAD);
        unpack(&archive(), &dir).unwrap();

        assert_eq!(
            std::fs::read(dir.join("bin/python3")).unwrap(),
            b"interpreter"
        );
        assert_eq!(
            std::fs::read(dir.join("lib/python3.13/os.py")).unwrap(),
            b"library"
        );
        assert!(!dir.join("build").exists() && !dir.join("PYTHON.json").exists());
        let beside: Vec<_> = std::fs::read_dir(cache.path().join("python"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(beside, [PAYLOAD], "the stage was left behind");
    }

    /// Two sessions starting at once both unpack; the second rename fails, and
    /// that is fine because the first one's tree is whole.
    #[test]
    fn losing_the_race_to_unpack_uses_the_winners_tree() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join(PAYLOAD);
        unpack(&archive(), &dir).unwrap();
        std::fs::write(dir.join("winner"), b"").unwrap();

        unpack(&archive(), &dir).unwrap();
        assert!(dir.join("winner").exists());
    }

    #[test]
    fn a_build_without_the_payload_says_what_would_supply_it() {
        let err = not_embedded().to_string();
        assert!(err.contains("built without its Python payload"), "{err}");
        assert!(err.contains("OUTRIG_PYTHON_ARCHIVE"), "{err}");
        assert!(err.contains(&format!("{PAYLOAD}.tar.zst")), "{err}");
    }
}
