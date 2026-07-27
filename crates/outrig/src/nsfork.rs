//! `fork(2)` + `setns(2)` plumbing, shared by the network interceptor and the
//! container user bootstrap.
//!
//! Joining a user namespace requires a single-threaded process, and tokio is
//! long since running by the time either caller needs one. So the work happens
//! in a forked child that talks back over a `SOCK_SEQPACKET` socketpair: an
//! 8-byte status payload, optionally carrying file descriptors as `SCM_RIGHTS`.
//!
//! Everything a child runs must be async-signal-safe. `fork()` in a
//! multi-threaded process gives the child a single thread but every lock the
//! other threads held, so a child that allocates can deadlock on the malloc
//! lock. The helpers here allocate nothing and return
//! [`io::Error::from_raw_os_error`] (which doesn't allocate either) rather than
//! [`io::Error::other`] on the paths a child can reach.

use std::io;
use std::os::fd::{FromRawFd as _, OwnedFd, RawFd};

use nix::libc;

/// The child's reply. `step` is zero on success; what a non-zero value means
/// is the caller's to define. Travels as `[step: u32, errno: i32]`,
/// little-endian -- a `SCM_RIGHTS` message needs a non-empty payload anyway,
/// so this rides along for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) step: u32,
    pub(crate) errno: i32,
}

impl Status {
    pub(crate) const OK: Status = Status { step: 0, errno: 0 };

    pub(crate) fn failed(step: u32, errno: i32) -> Self {
        Self { step, errno }
    }

    fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..4].copy_from_slice(&self.step.to_le_bytes());
        out[4..].copy_from_slice(&self.errno.to_le_bytes());
        out
    }

    fn from_bytes(b: [u8; 8]) -> Self {
        Self {
            step: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            errno: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        }
    }
}

/// Most descriptors a child may hand back in one message.
pub(crate) const MAX_FDS: usize = 2;

const CTRL_LEN: usize = cmsg_space(size_of::<[RawFd; MAX_FDS]>());

/// `msg_control` is walked as a `cmsghdr`, so the buffer behind it has to be
/// aligned for one -- a bare `[u8; N]` is 1-aligned.
#[repr(C, align(8))]
struct CmsgBuf([u8; CTRL_LEN]);

/// Run `child` in a forked child process and collect the message it sends back
/// over the socketpair, plus any descriptors attached to it. `child` receives
/// its end of the socket, must send exactly one message (see [`send_status`]),
/// and must stay async-signal-safe. Its exit code carries nothing: the parent
/// reads the [`Status`], or an error if the child died without one.
///
/// Errors only when the fork machinery itself failed or the child never
/// replied -- a child that reports a failure through its [`Status`] still
/// returns `Ok`.
pub(crate) fn fork_collect<F>(child: F) -> io::Result<(Status, Vec<OwnedFd>)>
where
    F: FnOnce(RawFd),
{
    let mut sv = [0 as RawFd; 2];
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, sv.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error());
    }

    let pid = unsafe { libc::fork() };
    if pid == -1 {
        let err = io::Error::last_os_error();
        close_fd(sv[0]);
        close_fd(sv[1]);
        return Err(err);
    }

    if pid == 0 {
        close_fd(sv[0]);
        child(sv[1]);
        // `_exit` diverges, so nothing in this frame is ever dropped.
        unsafe { libc::_exit(0) };
    }

    close_fd(sv[1]);
    let received = recv_status(sv[0]);
    close_fd(sv[0]);

    // Reaping our own child doesn't race tokio's process driver: it only
    // waits on pids it spawned itself.
    let mut wait_status = 0;
    let _ = unsafe { libc::waitpid(pid, &mut wait_status, 0) };

    received
}

/// Send one status message, optionally passing `fds` to the peer. Safe to call
/// from a forked child.
pub(crate) fn send_status(sock: RawFd, payload: Status, fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }

    let mut bytes = payload.to_bytes();
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    let mut control = CmsgBuf([0u8; CTRL_LEN]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    if !fds.is_empty() {
        let data_len = size_of_val(fds);
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = CTRL_LEN;
        unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null() {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = cmsg_len(data_len);
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg).cast::<u8>(),
                data_len,
            );
            msg.msg_controllen = (*cmsg).cmsg_len;
        }
    }

    if unsafe { libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Receive one status message and the descriptors that came with it. The
/// descriptors arrive owned, so a caller that drops them closes them.
fn recv_status(sock: RawFd) -> io::Result<(Status, Vec<OwnedFd>)> {
    let mut payload = [0u8; 8];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut control = CmsgBuf([0u8; CTRL_LEN]);
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    msg.msg_controllen = CTRL_LEN;

    let n = unsafe { libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n == -1 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "namespace helper exited without reporting a status",
        ));
    }

    let status = Status::from_bytes(payload);
    let mut out = Vec::new();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Ok((status, out));
        }
        if (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "namespace helper returned unexpected control data",
            ));
        }
        let data_len = (*cmsg).cmsg_len.saturating_sub(cmsg_len(0));
        let count = data_len / size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
        for i in 0..count {
            out.push(OwnedFd::from_raw_fd(*data.add(i)));
        }
    }
    Ok((status, out))
}

/// `setns(2)` as an [`io::Result`]. Safe to call from a forked child.
pub(crate) fn setns_raw(fd: RawFd, nstype: libc::c_int) -> io::Result<()> {
    let rc = unsafe { libc::setns(fd, nstype) };
    if rc == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn close_fd(fd: RawFd) {
    unsafe {
        libc::close(fd);
    }
}

const fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

const fn cmsg_space(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + cmsg_align(data_len)
}

const fn cmsg_len(data_len: usize) -> usize {
    cmsg_align(size_of::<libc::cmsghdr>()) + data_len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_step_and_errno() {
        let status = Status::failed(7, libc::EPERM);
        assert_eq!(Status::from_bytes(status.to_bytes()), status);
        assert_eq!(Status::from_bytes(Status::OK.to_bytes()), Status::OK);
    }

    #[test]
    fn fork_collect_carries_status_and_descriptors_back() {
        let (reply, fds) = fork_collect(|sock| {
            // A pipe read end is the cheapest descriptor to hand back.
            let mut pipe = [0 as RawFd; 2];
            if unsafe { libc::pipe(pipe.as_mut_ptr()) } == -1 {
                let _ = send_status(sock, Status::failed(1, 0), &[]);
                return;
            }
            let _ = send_status(sock, Status::OK, &pipe[..1]);
        })
        .expect("fork_collect");

        assert_eq!(reply, Status::OK);
        assert_eq!(fds.len(), 1, "the pipe read end should arrive owned");
    }

    #[test]
    fn fork_collect_reports_a_child_failure_without_descriptors() {
        let (reply, fds) = fork_collect(|sock| {
            let _ = send_status(sock, Status::failed(4, libc::EPERM), &[]);
        })
        .expect("fork_collect");

        assert_eq!(reply, Status::failed(4, libc::EPERM));
        assert!(fds.is_empty());
    }

    #[test]
    fn fork_collect_errors_when_the_child_never_replies() {
        let err = fork_collect(|_sock| {}).expect_err("silent child");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
