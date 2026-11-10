//! A hold on one process's identity, for acting on it through `/proc`.
//!
//! A PID is a name the kernel hands out again once its process is reaped, so
//! a number read from `podman inspect` and used for `/proc/<pid>/...` some time
//! later can land in an unrelated process. A [`ProcPin`] is a directory
//! descriptor opened on `/proc/<pid>` *once*: it refers to the process that
//! held the number at that moment and to nothing else. When that process is
//! gone, every lookup through the descriptor fails (`ESRCH`/`ENOENT`) instead
//! of resolving to whoever holds the number now, so a recycled PID cannot be
//! swapped in between the check and the use.

use std::ffi::CString;

use nix::libc;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// An open `/proc/<pid>` directory. See the [module docs](self).
#[derive(Debug)]
pub(crate) struct ProcPin {
    pid: u32,
    dir: OwnedFd,
}

impl ProcPin {
    /// Pins whatever process holds `pid` right now.
    ///
    /// Pinning proves nothing about *which* process that is: the caller decides
    /// that by reading it through the pin ([`parent_pid`](Self::parent_pid)),
    /// never by number.
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        let path = CString::new(format!("/proc/{pid}")).expect("digits only");
        // O_PATH: enough to resolve names relative to it, and it needs no
        // permission on the directory itself.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pid,
            dir: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    /// The number the process held when it was pinned. Good for display and for
    /// commands that can only take a number -- not for deciding who it is.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Opens `rel`, such as `ns/user`, read-only in the pinned process's `/proc`
    /// directory. Fails once the process is gone.
    pub(crate) fn open_file(&self, rel: &str) -> io::Result<File> {
        let rel = CString::new(rel).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let fd: RawFd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                rel.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            )
        };
        if fd == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    /// The pinned process's `/proc/<pid>/stat`. Opening a `/proc` file of a
    /// finished process succeeds and it is the *read* that fails, so this is
    /// also the test for whether the process is still there.
    pub(crate) fn stat(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_file("stat")?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// The pinned process's parent, as it says in `stat`. `None` when `stat`
    /// has no readable parent field.
    pub(crate) fn parent_pid(&self) -> io::Result<Option<u32>> {
        Ok(parent_pid_from_stat(&self.stat()?))
    }

    /// Whether the pinned process is still there to be acted on.
    pub(crate) fn ensure_live(&self) -> io::Result<()> {
        self.stat().map(|_| ())
    }
}

/// The parent PID in the bytes of a `/proc/<pid>/stat`. `comm`, the second
/// field, is the kernel's raw task name -- free bytes that may hold spaces,
/// `)`, or a multibyte character cut mid-sequence by the 15-byte limit -- so
/// the fields after it are found from the last `)` without decoding it.
pub(crate) fn parent_pid_from_stat(stat: &[u8]) -> Option<u32> {
    let after_comm = &stat[stat.iter().rposition(|&b| b == b')')? + 1..];
    std::str::from_utf8(after_comm)
        .ok()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn parent_pid_is_read_past_a_comm_with_parens_and_spaces() {
        assert_eq!(
            parent_pid_from_stat(b"1775180 (sleep) S 1775165 1 1 0"),
            Some(1775165)
        );
        assert_eq!(parent_pid_from_stat(b"42 (a b) c) S 7 1 1 0"), Some(7));
        assert_eq!(parent_pid_from_stat(b"42 (x) S"), None);
        assert_eq!(parent_pid_from_stat(b"no parens"), None);
    }

    #[test]
    fn parent_pid_is_read_past_a_comm_that_is_not_utf8() {
        // A multibyte name truncated to the kernel's 15 bytes ends mid-character.
        let mut stat = b"42 (".to_vec();
        stat.extend_from_slice(&[b'a'; 13]);
        stat.extend_from_slice(&"é".as_bytes()[..1]);
        stat.extend_from_slice(b") S 7 1 1 0");
        assert!(std::str::from_utf8(&stat).is_err());
        assert_eq!(parent_pid_from_stat(&stat), Some(7));
    }

    #[test]
    fn a_live_process_is_read_through_its_pin() {
        let pin = ProcPin::open(std::process::id()).expect("pin ourselves");
        assert_eq!(pin.pid(), std::process::id());
        pin.ensure_live().expect("we are running");
        assert_eq!(
            pin.parent_pid().expect("stat"),
            Some(std::os::unix::process::parent_id())
        );
        pin.open_file("ns/net").expect("our own network namespace");
    }

    #[test]
    fn a_pid_that_does_not_exist_cannot_be_pinned() {
        // pid 0 never has a `/proc` entry.
        assert!(ProcPin::open(0).is_err());
    }

    /// The point of the type: once the process is reaped, the pin reaches
    /// nothing, rather than whatever the number is later given to.
    #[test]
    fn a_pin_reaches_nothing_once_its_process_is_reaped() {
        let mut child = Command::new("true").spawn().expect("spawn true");
        let pin = ProcPin::open(child.id()).expect("pin the child");
        child.wait().expect("reap");

        let err = pin.ensure_live().expect_err("the process is gone");
        assert!(
            matches!(err.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)),
            "{err}"
        );
        let err = pin
            .open_file("ns/net")
            .expect_err("its namespaces are gone");
        assert!(
            matches!(err.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)),
            "{err}"
        );
    }
}
