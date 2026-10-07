//! The static CPython every session mounts, the pure-Python wheels unpacked
//! beside it, and where they live on the host.
//!
//! `build.rs` fetches the pinned `python-build-standalone` release, verifies
//! it, and embeds it. Unpacking sets the interpreter's GNU_STACK memory size
//! to 8 MiB, the default musl gives every thread in every child process. A
//! session's first start unpacks it under the user's cache directory, and every
//! session binds that tree read-only at [`PAYLOAD_MOUNT`]. Nothing falls back
//! to whatever `python3` the image happens to carry, which would be a different
//! interpreter with a different library, or none at all.
//!
//! RPyC, which carries hosted-object requests between the interpreter and each
//! binding process, arrives the same way: `build.rs` fetches the pinned wheel,
//! lays its members out as a tar, and embeds that, and [`rpyc_dir`] unpacks it
//! once into a directory named for the pin. A binding process imports RPyC
//! from that directory, and `0003-21` mounts it read-only in the container
//! beside the payload.

use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use nix::sys::statvfs::{FsFlags, statvfs};

use super::stack::{THREAD_STACK, stack_field};
use crate::container::ContainerMount;
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
/// to with a `-stack8m` suffix for the unpack-time patch -- so a new pin or
/// patch unpacks beside an old tree rather than over it.
pub(super) const PAYLOAD: &str = env!("OUTRIG_PYTHON_PAYLOAD");

/// The part of the payload archive that is the interpreter: `python/build`
/// beside it is the build's leftovers.
const PAYLOAD_SUBTREE: &str = "python/install";

/// The RPyC wheel's members, as `build.rs` laid them out under the pin's name;
/// empty when that build could not fetch the wheel.
#[cfg_attr(not(test), allow(dead_code))]
static RPYC_ARCHIVE: &[u8] = include_bytes!(concat!(
    env!("OUT_DIR"),
    "/",
    env!("OUTRIG_RPYC_DIR"),
    ".tar.zst"
));

/// The wheel's name less `.whl`, the directory it unpacks to and the root of
/// every member in [`RPYC_ARCHIVE`].
pub(super) const RPYC: &str = env!("OUTRIG_RPYC_DIR");

/// The payload's directory on the host, ready to bind at [`PAYLOAD_MOUNT`],
/// unpacked from the embedded archive the first time it is asked for.
///
/// Built for the *target* architecture. podman will run a foreign-arch image
/// under emulation without saying so, and this does not detect that; see
/// #285, which covers the launcher's half and, in its comments, this one.
pub(crate) async fn host_dir() -> Result<PathBuf> {
    if ARCHIVE.is_empty() {
        return Err(not_embedded());
    }
    let dir = cache_dir("python payload")?
        .join("outrig/python")
        .join(format!("{PAYLOAD}-stack8m"));
    if dir.is_dir() {
        runnable_from(&dir)?;
        return Ok(dir);
    }
    // The blocking task, not this future, holds the lock through the unpack,
    // so a launch cancelled while it waits cannot let another start a second.
    let target = dir.clone();
    tokio::task::spawn_blocking(move || {
        let parent = target.parent().expect("the payload directory has a parent");
        std::fs::create_dir_all(parent).path_ctx("create", parent)?;
        runnable_from(parent)?;
        unpack_once(ARCHIVE, &target, PAYLOAD_SUBTREE)
    })
    .await
    .map_err(|e| OutrigError::Io(std::io::Error::other(e)))??;
    Ok(dir)
}

/// The vendored RPyC's directory on the host, holding the `rpyc` package,
/// unpacked from the embedded wheel the first time it is asked for. Binding
/// processes import from it, and `0003-21` mounts it read-only in the
/// container beside the payload.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn rpyc_dir() -> Result<PathBuf> {
    if RPYC_ARCHIVE.is_empty() {
        return Err(wheel_not_embedded());
    }
    let dir = cache_dir("RPyC wheel")?.join("outrig/wheels").join(RPYC);
    if dir.is_dir() {
        return Ok(dir);
    }
    let target = dir.clone();
    tokio::task::spawn_blocking(move || unpack_once(RPYC_ARCHIVE, &target, RPYC))
        .await
        .map_err(|e| OutrigError::Io(std::io::Error::other(e)))??;
    Ok(dir)
}

/// The user's cache directory, or a configuration error naming `what` could
/// not be placed.
fn cache_dir(what: &str) -> Result<PathBuf> {
    cache_root(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME")).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "cannot place the {what}: neither an absolute XDG_CACHE_HOME nor HOME is set"
        ))
    })
}

/// The payload's mount: [`host_dir`], bound read-only at [`PAYLOAD_MOUNT`],
/// as every session's primary container gets it.
pub(crate) async fn mount() -> Result<ContainerMount> {
    Ok(ContainerMount::shared_read_only(
        host_dir().await?,
        PAYLOAD_MOUNT,
    ))
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

/// Unpack `archive`'s `subtree` to `dir` unless another thread or process has
/// by the time this one holds the lock. One payload unpack decodes through a
/// 128 MiB window and writes about 170 MB, so concurrent first launches must
/// not each do it.
fn unpack_once(archive: &[u8], dir: &Path, subtree: &str) -> Result<()> {
    let parent = dir.parent().expect("the unpack directory has a parent");
    std::fs::create_dir_all(parent).path_ctx("create", parent)?;
    let lock = lock_path(dir);
    let file = std::fs::File::create(&lock).path_ctx("create", &lock)?;
    let _held = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| std::io::Error::from(errno))
        .path_ctx("lock", &lock)?;
    if dir.is_dir() {
        return Ok(());
    }
    unpack(archive, dir, subtree)
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

/// As [`not_embedded`], for the RPyC wheel; `build.rs` records why in
/// `OUTRIG_RPYC_UNAVAILABLE_REASON`.
fn wheel_not_embedded() -> OutrigError {
    let reason = option_env!("OUTRIG_RPYC_UNAVAILABLE_REASON").unwrap_or("unknown");
    OutrigError::Configuration(format!(
        "this outrig was built without its RPyC wheel: {reason}\n\
         help: rebuild with network access, or with OUTRIG_RPYC_WHEEL naming a copy of {RPYC}.whl"
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

/// Unpack `archive`'s `subtree` to `dir`. It goes to a sibling first and is
/// renamed into place once complete, so `dir` either holds a whole tree or
/// does not exist. [`unpack_once`] keeps two from racing; should one still
/// lose a rename, it uses the winner's tree.
fn unpack(archive: &[u8], dir: &Path, subtree: &str) -> Result<()> {
    let parent = dir.parent().expect("the unpack directory has a parent");
    std::fs::create_dir_all(parent).path_ctx("create", parent)?;
    let stage = tempfile::Builder::new()
        .prefix(".unpack-")
        .tempdir_in(parent)
        .path_ctx("create a directory in", parent)?;
    let at = stage.path();
    fn unpacking<T>(result: std::io::Result<T>, at: &Path) -> Result<T> {
        result.path_ctx("unpack into", at)
    }

    // No window limit, as in `build.rs`: these bytes matched the pinned digest
    // before they were embedded, and the payload's archive needs 128 MiB.
    let decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(archive, u64::MAX)
        .map_err(std::io::Error::other);
    let mut tar = tar::Archive::new(unpacking(decoder, at)?);
    for entry in unpacking(tar.entries(), at)? {
        let mut entry = unpacking(entry, at)?;
        if !entry.path().is_ok_and(|path| path.starts_with(subtree)) {
            continue;
        }
        // `unpack_in` refuses a member that would land outside the stage and
        // says so by returning `false`; a tree missing one is not whole.
        if !unpacking(entry.unpack_in(at), at)? {
            return Err(OutrigError::Configuration(format!(
                "the embedded archive has a member outside its tree: {}",
                String::from_utf8_lossy(&entry.path_bytes())
            )));
        }
    }

    if subtree == PAYLOAD_SUBTREE {
        patch_thread_stack(&at.join(subtree).join("bin/python3"))?;
    }

    match std::fs::rename(stage.path().join(subtree), dir) {
        Ok(()) => Ok(()),
        Err(_) if dir.is_dir() => Ok(()),
        Err(e) => Err(e).path_ctx("move the unpacked tree to", dir),
    }
}

/// Patch the staged interpreter before its directory becomes visible. The
/// build checked this same field after verifying the archive's pinned digest.
fn patch_thread_stack(path: &Path) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .path_ctx("open for the thread-stack patch", path)?;
    let mut head = Vec::new();
    (&mut file)
        .take(64 * 1024)
        .read_to_end(&mut head)
        .path_ctx("read the ELF headers from", path)?;
    let field = stack_field(&head).map_err(|why| {
        OutrigError::Configuration(format!(
            "cannot set the thread stack in {}: {why}",
            path.display()
        ))
    })?;
    if head[field..field + 8] != THREAD_STACK.to_le_bytes() {
        file.seek(SeekFrom::Start(field as u64))
            .path_ctx("seek in", path)?;
        file.write_all(&THREAD_STACK.to_le_bytes())
            .path_ctx("patch the thread stack in", path)?;
    }
    Ok(())
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
        let elf = super::super::stack::stack_tests::elf();
        for (path, bytes) in [
            ("python/PYTHON.json", &b"{}"[..]),
            ("python/build/Modules/Setup", b"build only"),
            ("python/install/bin/python3.13", &elf[..]),
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

    #[tokio::test]
    async fn the_real_payload_has_an_eight_mib_gnu_stack() {
        let dir = host_dir().await.unwrap();
        assert!(
            dir.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-stack8m")
        );
        let head = std::fs::read(dir.join("bin/python3.13")).unwrap();
        let field = stack_field(&head).unwrap();
        assert_eq!(&head[field..field + 8], &THREAD_STACK.to_le_bytes());
    }

    #[test]
    fn patching_changes_only_the_stack_field_and_is_idempotent() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let original = super::super::stack::stack_tests::elf();
        std::fs::write(file.path(), &original).unwrap();
        patch_thread_stack(file.path()).unwrap();
        let patched = std::fs::read(file.path()).unwrap();
        assert_eq!(&patched[..104], &original[..104]);
        assert_eq!(&patched[104..112], &THREAD_STACK.to_le_bytes());
        assert_eq!(&patched[112..], &original[112..]);
        patch_thread_stack(file.path()).unwrap();
        assert_eq!(std::fs::read(file.path()).unwrap(), patched);
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
            std::thread::spawn(move || unpack_once(b"not an archive", &dir, PAYLOAD_SUBTREE))
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
        unpack(&archive(), &dir, PAYLOAD_SUBTREE).unwrap();

        let mut expected = super::super::stack::stack_tests::elf();
        expected[104..112].copy_from_slice(&THREAD_STACK.to_le_bytes());
        assert_eq!(std::fs::read(dir.join("bin/python3")).unwrap(), expected);
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
        unpack(&archive(), &dir, PAYLOAD_SUBTREE).unwrap();
        std::fs::write(dir.join("winner"), b"").unwrap();

        unpack(&archive(), &dir, PAYLOAD_SUBTREE).unwrap();
        assert!(dir.join("winner").exists());
    }

    /// A wheel's tar, as `build.rs` lays one out: every member under the pin.
    fn wheel_archive() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, bytes) in [
            (format!("{RPYC}/rpyc/__init__.py"), &b"package"[..]),
            (
                format!("{RPYC}/rpyc-6.0.2.dist-info/WHEEL"),
                b"Wheel-Version: 1.0\n",
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest)
    }

    #[test]
    fn a_wheel_unpacks_under_its_pin() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("wheels").join(RPYC);
        unpack_once(&wheel_archive(), &dir, RPYC).unwrap();
        assert_eq!(
            std::fs::read(dir.join("rpyc/__init__.py")).unwrap(),
            b"package"
        );
        assert!(dir.join("rpyc-6.0.2.dist-info/WHEEL").exists());
        let beside: Vec<_> = std::fs::read_dir(cache.path().join("wheels"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lock"))
            .collect();
        assert_eq!(beside, [RPYC], "the stage was left behind");
    }

    #[test]
    fn a_build_without_the_wheel_says_what_would_supply_it() {
        let err = wheel_not_embedded().to_string();
        assert!(err.contains("built without its RPyC wheel"), "{err}");
        assert!(err.contains("OUTRIG_RPYC_WHEEL"), "{err}");
        assert!(err.contains(&format!("{RPYC}.whl")), "{err}");
    }

    #[test]
    fn a_build_without_the_payload_says_what_would_supply_it() {
        let err = not_embedded().to_string();
        assert!(err.contains("built without its Python payload"), "{err}");
        assert!(err.contains("OUTRIG_PYTHON_ARCHIVE"), "{err}");
        assert!(err.contains(&format!("{PAYLOAD}.tar.zst")), "{err}");
    }
}
