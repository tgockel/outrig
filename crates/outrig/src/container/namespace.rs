//! Entering a container's namespaces from the host to bootstrap its user.
//!
//! The ordering is forced. A forked child joins the container's *user*
//! namespace first -- `setns(CLONE_NEWUSER)` grants a full capability set in
//! the namespace it joins, so the child can then `setuid(0)` and become the
//! container's root -- and only then its mount namespace. Joining the mount
//! namespace straight from the host is EPERM.
//!
//! Two passes, because a forked child of a live tokio process must stay to
//! raw syscalls (see [`crate::nsfork`]) and so cannot parse anything:
//!
//! 1. [`open_user_db`] hands back open descriptors for `/etc/passwd` and
//!    `/etc/group`. The kernel checks permission at `open`, not at `write`, so
//!    the parent can then read and append through them with ordinary code even
//!    though it is an unprivileged host process.
//! 2. [`create_home`] creates and `chown`s the home directory, which cannot be
//!    expressed as a descriptor handed back out.
//!
//! `/etc/shadow` is deliberately not written. Nothing in OutRig authenticates
//! as this user, `getpwnam` never consults shadow, and the entry `useradd`
//! writes is a locked (`!`) password -- so `su` and `sudo` fail identically
//! with or without it. It would also embed a date, which no test could pin.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::fd::RawFd;
use std::path::Path;

use nix::libc;

use crate::nsfork;

/// Where the child stopped. The numbering is the wire format between the
/// child and its parent, so the values are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum NsStep {
    OpenUserNsFile = 1,
    OpenMountNsFile = 2,
    Fork = 3,
    SetnsUser = 4,
    SetIds = 5,
    SetnsMount = 6,
    OpenPasswd = 7,
    OpenGroup = 8,
    Reply = 9,
    Mkdir = 10,
    Chown = 11,
    OpenHome = 12,
}

impl NsStep {
    /// Every step, in discriminant order -- the one place the wire codes are
    /// listed, so [`NsStep::from_code`] cannot drift from the enum.
    const ALL: [NsStep; 12] = [
        NsStep::OpenUserNsFile,
        NsStep::OpenMountNsFile,
        NsStep::Fork,
        NsStep::SetnsUser,
        NsStep::SetIds,
        NsStep::SetnsMount,
        NsStep::OpenPasswd,
        NsStep::OpenGroup,
        NsStep::Reply,
        NsStep::Mkdir,
        NsStep::Chown,
        NsStep::OpenHome,
    ];

    fn from_code(code: u32) -> Option<Self> {
        let index = usize::try_from(code).ok()?.checked_sub(1)?;
        NsStep::ALL.get(index).copied()
    }

    /// Human-readable step name, used in error messages.
    pub(super) fn label(self) -> &'static str {
        match self {
            NsStep::OpenUserNsFile => "open /proc/<pid>/ns/user",
            NsStep::OpenMountNsFile => "open /proc/<pid>/ns/mnt",
            NsStep::Fork => "fork",
            NsStep::SetnsUser => "setns(CLONE_NEWUSER)",
            NsStep::SetIds => "setuid(0)",
            NsStep::SetnsMount => "setns(CLONE_NEWNS)",
            NsStep::OpenPasswd => "open /etc/passwd",
            NsStep::OpenGroup => "open /etc/group",
            NsStep::Reply => "hand back the opened files",
            NsStep::Mkdir => "mkdir the home directory",
            NsStep::Chown => "chown the home directory",
            NsStep::OpenHome => "open the home directory",
        }
    }
}

/// A failure entering the namespace or acting inside it.
#[derive(Debug, Clone, Copy)]
pub(super) struct NsError {
    pub(super) step: NsStep,
    errno: i32,
}

impl NsError {
    fn new(step: NsStep, err: &io::Error) -> Self {
        Self {
            step,
            errno: err.raw_os_error().unwrap_or(0),
        }
    }

    /// `step`, with the errno of the syscall that just failed. Safe to call
    /// from a forked child.
    fn last(step: NsStep) -> Self {
        Self {
            step,
            errno: errno(),
        }
    }

    pub(super) fn io(self) -> io::Error {
        io::Error::from_raw_os_error(self.errno)
    }
}

impl std::fmt::Display for NsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step.label(), self.io())
    }
}

/// Which of the two databases a handle refers to.
#[derive(Debug, Clone, Copy)]
pub(super) enum Db {
    Passwd,
    Group,
}

impl Db {
    pub(super) fn path(self) -> &'static str {
        match self {
            Db::Passwd => "/etc/passwd",
            Db::Group => "/etc/group",
        }
    }
}

/// The container's user databases, opened inside its mount namespace by a
/// child holding container-root credentials.
#[derive(Debug)]
pub(super) struct UserDb {
    passwd: File,
    group: File,
}

impl UserDb {
    fn file(&self, which: Db) -> &File {
        match which {
            Db::Passwd => &self.passwd,
            Db::Group => &self.group,
        }
    }

    /// Current contents, read from the start without disturbing where the
    /// next append lands (the descriptor carries `O_APPEND`, so writes always
    /// go to the end regardless of this offset). Lossy-decoded: both files are
    /// ASCII in every image that has them.
    pub(super) fn read(&self, which: Db) -> io::Result<String> {
        let mut file = self.file(which);
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Append `blob` at end of file. Appending in place, rather than writing a
    /// replacement and renaming it over, is what keeps the file's owner and
    /// mode -- a replacement would land owned by whoever wrote it.
    pub(super) fn append(&self, which: Db, blob: &[u8]) -> io::Result<()> {
        let mut file = self.file(which);
        file.write_all(blob)?;
        file.flush()
    }
}

/// Pass 1: open the container's user databases from inside its namespaces.
/// Reads nothing and writes nothing -- on any failure the container is exactly
/// as it was.
pub(super) fn open_user_db(pid: u32) -> Result<UserDb, NsError> {
    let ns = NsFiles::open(pid)?;
    let passwd = CString::new(Db::Passwd.path()).expect("static path");
    let group = CString::new(Db::Group.path()).expect("static path");

    let (status, fds) = nsfork::fork_collect(|sock| {
        if let Err(step) = enter(&ns) {
            reply_failure(sock, step);
            return;
        }
        // O_RDWR (not O_WRONLY): the parent reads through the same fd.
        let passwd_fd = unsafe { libc::open(passwd.as_ptr(), libc::O_RDWR | libc::O_APPEND) };
        if passwd_fd == -1 {
            reply_failure(sock, NsStep::OpenPasswd);
            return;
        }
        let group_fd = unsafe { libc::open(group.as_ptr(), libc::O_RDWR | libc::O_APPEND) };
        if group_fd == -1 {
            reply_failure(sock, NsStep::OpenGroup);
            return;
        }
        let _ = nsfork::send_status(sock, nsfork::Status::OK, &[passwd_fd, group_fd]);
    })
    .map_err(|e| NsError::new(NsStep::Fork, &e))?;

    check_reply(status)?;
    let mut fds = fds.into_iter();
    match (fds.next(), fds.next()) {
        (Some(passwd), Some(group)) => Ok(UserDb {
            passwd: File::from(passwd),
            group: File::from(group),
        }),
        // A success status with the wrong descriptor count can't happen
        // unless the wire protocol changed under us; the fds we did get are
        // owned, so returning drops them.
        _ => Err(NsError {
            step: NsStep::Reply,
            errno: libc::EPROTO,
        }),
    }
}

/// Pass 2: `mkdir -p` the home directory inside the container -- stricter
/// about the home directory itself, see [`make_home`] -- and give it to
/// `uid`:`gid`. Created inside the container's user namespace it would
/// otherwise belong to the container's root.
pub(super) fn create_home(pid: u32, home: &Path, uid: u32, gid: u32) -> Result<(), NsError> {
    let ns = NsFiles::open(pid)?;
    let dirs = mkdir_p_paths(home)?;

    let (status, _fds) = nsfork::fork_collect(|sock| {
        if let Err(step) = enter(&ns) {
            reply_failure(sock, step);
            return;
        }
        let status = match make_home(&dirs, uid, gid) {
            Ok(()) => nsfork::Status::OK,
            Err(e) => nsfork::Status::failed(e.step as u32, e.errno),
        };
        let _ = nsfork::send_status(sock, status, &[]);
    })
    .map_err(|e| NsError::new(NsStep::Fork, &e))?;

    check_reply(status)
}

/// Every path [`make_home`] needs, laid out before the fork, outermost first:
/// "/home", then "/home/<user>". This is `mkdir -p`, unrolled.
fn mkdir_p_paths(home: &Path) -> Result<Vec<CString>, NsError> {
    let mut dirs = Vec::new();
    for ancestor in home.ancestors() {
        if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
            break;
        }
        dirs.push(cstring(ancestor)?);
    }
    dirs.reverse();
    Ok(dirs)
}

/// Create each of `dirs` in turn, then give the last of them -- the home
/// directory -- to `uid`:`gid`. Runs in the forked child: syscalls only.
///
/// Anything already at an ancestor is fine: `/home` as a symlink to a
/// directory is a normal layout, and a non-directory fails the next `mkdir`
/// with `ENOTDIR`. The home directory itself is where this departs from
/// `mkdir -p`: whatever is there must be a directory, not a file, a FIFO, or
/// a symlink even to a directory. Anything else would be passed off as
/// `$HOME` to every exec, and a symlink would carry the `chown` to wherever it
/// points -- the bind-mounted workspace included. Opening with `O_DIRECTORY |
/// O_NOFOLLOW` and changing ownership through that descriptor settles both in
/// one step, with no window between the check and the `chown`.
fn make_home(dirs: &[CString], uid: u32, gid: u32) -> Result<(), NsError> {
    for dir in dirs {
        let rc = unsafe { libc::mkdir(dir.as_ptr(), 0o755) };
        if rc == -1 && errno() != libc::EEXIST {
            return Err(NsError::last(NsStep::Mkdir));
        }
    }
    // Only a home of `/` itself lays out no paths at all.
    let Some(home) = dirs.last() else {
        return Err(NsError {
            step: NsStep::Mkdir,
            errno: libc::EINVAL,
        });
    };
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(home.as_ptr(), flags) };
    if fd == -1 {
        return Err(NsError::last(NsStep::OpenHome));
    }
    let result = if unsafe { libc::fchown(fd, uid, gid) } == -1 {
        Err(NsError::last(NsStep::Chown))
    } else {
        Ok(())
    };
    nsfork::close_fd(fd);
    result
}

/// The namespace files, opened in the parent so that a failure to open them
/// never involves a child at all.
struct NsFiles {
    user: File,
    mount: File,
}

impl NsFiles {
    fn open(pid: u32) -> Result<Self, NsError> {
        let user = File::open(format!("/proc/{pid}/ns/user"))
            .map_err(|e| NsError::new(NsStep::OpenUserNsFile, &e))?;
        let mount = File::open(format!("/proc/{pid}/ns/mnt"))
            .map_err(|e| NsError::new(NsStep::OpenMountNsFile, &e))?;
        Ok(Self { user, mount })
    }
}

/// Join the container's namespaces. Runs in the forked child: syscalls only.
fn enter(ns: &NsFiles) -> Result<(), NsStep> {
    use std::os::fd::AsRawFd as _;

    // EINVAL means the container has no user namespace of its own -- rootful
    // podman -- so we are already where we need to be and are already root.
    match nsfork::setns_raw(ns.user.as_raw_fd(), libc::CLONE_NEWUSER) {
        Ok(()) => {
            // Joining granted a full capability set in that namespace; spend
            // it on becoming the container's root, which owns /etc.
            if unsafe { libc::setgid(0) } == -1 || unsafe { libc::setuid(0) } == -1 {
                return Err(NsStep::SetIds);
            }
        }
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {}
        Err(_) => return Err(NsStep::SetnsUser),
    }

    nsfork::setns_raw(ns.mount.as_raw_fd(), libc::CLONE_NEWNS).map_err(|_| NsStep::SetnsMount)?;
    Ok(())
}

/// Report `step`, with the errno of the syscall that just failed, to the
/// parent. Runs in the forked child: syscalls only.
fn reply_failure(sock: RawFd, step: NsStep) {
    let _ = nsfork::send_status(sock, nsfork::Status::failed(step as u32, errno()), &[]);
}

fn check_reply(status: nsfork::Status) -> Result<(), NsError> {
    if status.step == 0 {
        return Ok(());
    }
    let step = NsStep::from_code(status.step).ok_or(NsError {
        step: NsStep::Reply,
        errno: libc::EPROTO,
    })?;
    Err(NsError {
        step,
        errno: status.errno,
    })
}

/// A path the child can pass to `mkdir`/`chown`. Fails only on an interior
/// NUL, which a name read out of `/etc/passwd` cannot contain and a
/// [`sanitized`](super::userdb::sanitize_name) one cannot either -- but the
/// name may come from the container's own file, so this stays a `Result`
/// rather than an `expect`.
fn cstring(path: &Path) -> Result<CString, NsError> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| NsError {
        step: NsStep::Mkdir,
        errno: libc::EINVAL,
    })
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_codes_round_trip() {
        for (code, step) in (1u32..).zip(NsStep::ALL) {
            assert_eq!(step as u32, code);
            assert_eq!(NsStep::from_code(code), Some(step));
            assert!(!step.label().is_empty());
        }
        assert!(NsStep::from_code(0).is_none());
        assert!(NsStep::from_code(NsStep::ALL.len() as u32 + 1).is_none());
    }

    /// Entering our *own* namespaces exercises the whole round trip without a
    /// container: the user-namespace join returns EINVAL (already there, not a
    /// failure), and the mount-namespace join then needs CAP_SYS_ADMIN in the
    /// namespace that owns it, which an unprivileged process does not have.
    #[test]
    fn entry_failure_reports_the_step_and_errno() {
        if nix::unistd::Uid::effective().is_root() {
            return; // root can join its own mount namespace; nothing to assert
        }
        let err = open_user_db(std::process::id()).expect_err("own mount namespace");
        assert_eq!(err.step, NsStep::SetnsMount, "unexpected step: {err}");
        assert_eq!(err.io().raw_os_error(), Some(libc::EPERM), "{err}");
    }

    #[test]
    fn a_missing_container_fails_before_forking() {
        // pid 0 never has /proc entries, so this stops at the parent's open.
        let err = open_user_db(0).expect_err("no such process");
        assert_eq!(err.step, NsStep::OpenUserNsFile);
    }

    /// What the child does once inside the namespaces, run in-process against
    /// `home` -- a stand-in for `/home/<user>` under a tempdir -- and giving
    /// it to ourselves, which needs no privilege.
    fn make_home_at(home: &Path) -> Result<(), NsError> {
        let (uid, gid) = own_ids();
        make_home(&mkdir_p_paths(home).expect("paths"), uid, gid)
    }

    fn own_ids() -> (u32, u32) {
        (
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw(),
        )
    }

    #[test]
    fn an_absent_home_is_created_and_chowned() {
        use std::os::unix::fs::MetadataExt as _;

        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home/dev");
        make_home_at(&home).expect("absent home");
        let meta = std::fs::symlink_metadata(&home).expect("stat home");
        assert!(meta.is_dir());
        assert_eq!((meta.uid(), meta.gid()), own_ids());

        // Finding it already there on a second run is fine.
        make_home_at(&home).expect("existing home");
    }

    /// `/home` itself a symlink to a directory is a normal distro layout.
    #[test]
    fn a_symlinked_parent_is_followed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("real")).expect("mkdir");
        std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("home"))
            .expect("symlink");
        make_home_at(&tmp.path().join("home/dev")).expect("home under a symlink");
        assert!(tmp.path().join("real/dev").is_dir());
    }

    #[test]
    fn a_home_that_is_not_a_directory_is_refused() {
        use std::os::unix::fs::symlink;

        /// Plants one shape at the given path, and anything it points to
        /// beside it.
        type Plant = fn(&Path);
        let shapes: [(&str, Plant); 5] = [
            ("regular file", |at| std::fs::write(at, "").expect("write")),
            ("fifo", |at| {
                nix::unistd::mkfifo(at, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
            }),
            ("symlink to a file", |at| {
                std::fs::write(at.with_file_name("target"), "").expect("write");
                symlink(at.with_file_name("target"), at).expect("symlink");
            }),
            ("symlink to a directory", |at| {
                std::fs::create_dir(at.with_file_name("target")).expect("mkdir");
                symlink(at.with_file_name("target"), at).expect("symlink");
            }),
            ("dangling symlink", |at| {
                symlink(at.with_file_name("nowhere"), at).expect("symlink");
            }),
        ];

        for (shape, plant) in shapes {
            let tmp = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(tmp.path().join("home")).expect("mkdir");
            let home = tmp.path().join("home/dev");
            plant(&home);
            let planted = std::fs::symlink_metadata(&home).expect("stat").file_type();

            let err = make_home_at(&home).expect_err(shape);
            assert_eq!(err.step, NsStep::OpenHome, "{shape}: {err}");
            let errno = err.io().raw_os_error();
            assert_eq!(errno, Some(libc::ENOTDIR), "{shape}: {err}");
            let after = std::fs::symlink_metadata(&home).expect("stat").file_type();
            assert_eq!(
                after, planted,
                "{shape}: the home path should be left as it was"
            );
        }
    }

    #[test]
    fn a_parent_that_is_not_a_directory_fails_the_mkdir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("home"), "").expect("write");
        let err = make_home_at(&tmp.path().join("home/dev")).expect_err("file at /home");
        assert_eq!(err.step, NsStep::Mkdir, "{err}");
        assert_eq!(err.io().raw_os_error(), Some(libc::ENOTDIR), "{err}");
    }
}
