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
}

impl NsStep {
    /// Every step, in discriminant order -- the one place the wire codes are
    /// listed, so [`NsStep::from_code`] cannot drift from the enum.
    const ALL: [NsStep; 11] = [
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

/// Pass 2: `mkdir -p` the home directory inside the container and give it to
/// `uid`:`gid`. Created inside the container's user namespace it would
/// otherwise belong to the container's root.
pub(super) fn create_home(pid: u32, home: &Path, uid: u32, gid: u32) -> Result<(), NsError> {
    let ns = NsFiles::open(pid)?;
    // Every path the child needs, laid out before the fork, outermost first:
    // "/home", then "/home/<user>". This is `mkdir -p`, unrolled.
    let mut dirs = Vec::new();
    for ancestor in home.ancestors() {
        if ancestor == Path::new("/") || ancestor.as_os_str().is_empty() {
            break;
        }
        dirs.push(cstring(ancestor)?);
    }
    dirs.reverse();
    let leaf = cstring(home)?;

    let (status, _fds) = nsfork::fork_collect(|sock| {
        if let Err(step) = enter(&ns) {
            reply_failure(sock, step);
            return;
        }
        for dir in &dirs {
            let rc = unsafe { libc::mkdir(dir.as_ptr(), 0o755) };
            if rc == -1 && errno() != libc::EEXIST {
                reply_failure(sock, NsStep::Mkdir);
                return;
            }
        }
        if unsafe { libc::chown(leaf.as_ptr(), uid, gid) } == -1 {
            reply_failure(sock, NsStep::Chown);
            return;
        }
        let _ = nsfork::send_status(sock, nsfork::Status::OK, &[]);
    })
    .map_err(|e| NsError::new(NsStep::Fork, &e))?;

    check_reply(status)
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
        for code in 1..=11 {
            let step = NsStep::from_code(code).expect("known step");
            assert_eq!(step as u32, code);
            assert!(!step.label().is_empty());
        }
        assert!(NsStep::from_code(0).is_none());
        assert!(NsStep::from_code(99).is_none());
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
}
