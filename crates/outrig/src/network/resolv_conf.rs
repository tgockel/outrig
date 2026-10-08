//! The container's `/etc/resolv.conf`, read and written by outrig itself.
//!
//! Interception points a container's resolver at its DNS listener and puts the
//! old one back afterwards. This used to be a shell run through `nsenter -U -m`,
//! and entering the mount namespace made that shell -- and the `cat` and
//! `printf` it ran -- the container image's own binaries. They ran with
//! outrig's whole environment, in the host's network and pid namespaces, before
//! any policy applied (#328). Nothing here `exec`s. A forked child enters the
//! container through [`nsfork::UserMountNs`], opens the file as the container's
//! root, and hands the descriptor back; every read and write after that is this
//! process's own code acting on that descriptor. The kernel checks permission
//! at `open`, so the parent needs no privilege to use it.
//!
//! The descriptor is also the resolver's identity. A pid is how the child gets
//! into the container, and the kernel hands pids out again; a held descriptor
//! names one inode for as long as it is held, so a restore issued late cannot
//! be carried into whatever holds that pid by then.

#[cfg(test)]
use std::ffi::CString;
use std::ffi::{CStr, OsStr};
use std::fmt::Display;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{FileExt as _, MetadataExt as _};
use std::path::Path;

use nix::libc;

use crate::error::{IoPathExt as _, OutrigError, Result};
use crate::nsfork::{self, EnterStep, NsFile, Status};

/// Where the resolver is, inside the container.
const RESOLV_CONF: &CStr = c"/etc/resolv.conf";

/// The largest resolver an attach will hold a copy of to put back.
///
/// A bound on what the container can make outrig read into memory, and nothing
/// subtler: a resolver file anywhere near this size is not a resolver file.
const MAX_RESOLV_SNAPSHOT: usize = 64 * 1024;

/// [`RESOLV_CONF`], as what this reports names it.
fn path() -> &'static Path {
    Path::new(OsStr::from_bytes(RESOLV_CONF.to_bytes()))
}

/// Where a resolver lives.
#[derive(Debug, Clone)]
pub(super) enum Place {
    /// `/etc/resolv.conf` in the container whose init holds this pid.
    Container(u32),
    /// A file on this host: the same child, entering nothing. Test-only on
    /// purpose, so no production path can name the host's own resolver.
    #[cfg(test)]
    Host(CString),
}

impl Place {
    #[cfg(test)]
    pub(super) fn host(path: &Path) -> Self {
        Place::Host(CString::new(path.as_os_str().as_bytes()).expect("a path with no NUL"))
    }
}

/// A container's resolver, as attach found it: open, and read.
#[derive(Debug)]
pub(super) struct Resolver {
    place: Place,
    /// `None` when the container has no resolver file at all.
    file: Option<File>,
    /// What the file held, to put back.
    original: Option<Vec<u8>>,
}

impl Resolver {
    /// Opens the resolver at `place` read-write, from inside its namespaces,
    /// and reads it.
    ///
    /// Refuses the shapes nothing could put back. A symbolic link to nothing:
    /// installing follows it and creates its target, and undoing that removes
    /// the link. Anything but a regular file: reading a FIFO blocks for as long
    /// as nothing writes it, and opening a device can do whatever that device
    /// does on open, so neither is opened at all. And a file too large to be a
    /// resolver, which this would otherwise hold in memory until the undo, or
    /// too large for this process to write back: see [`past_file_size_limit`].
    pub(super) fn open(place: Place, container: &str) -> Result<Self> {
        let (status, fds) = run_in(&place, probe).map_err(|e| refuse(container, e))?;
        if status != Status::OK {
            return Err(match Step::from_code(status.step) {
                Some(Step::Dangling) => refuse(
                    container,
                    "/etc/resolv.conf is a symbolic link to a file that does not exist. \
                     Installing would follow the link and create its target, and undoing \
                     that would remove the link itself",
                ),
                Some(Step::NotRegular) => refuse(
                    container,
                    "/etc/resolv.conf is not a regular file, so it is not one that could \
                     be read and put back",
                ),
                _ => refuse(container, step_failure(status)),
            });
        }
        let file = fds.into_iter().next().map(File::from);
        let original = match &file {
            None => None,
            Some(file) => {
                let bytes = read_at_most(file, MAX_RESOLV_SNAPSHOT + 1).map_err(|e| {
                    refuse(container, format!("could not read /etc/resolv.conf: {e}"))
                })?;
                if bytes.len() > MAX_RESOLV_SNAPSHOT {
                    return Err(refuse(
                        container,
                        format!(
                            "/etc/resolv.conf is past the {MAX_RESOLV_SNAPSHOT} bytes an \
                             attach will hold to put back"
                        ),
                    ));
                }
                if let Some(limit) = past_file_size_limit(bytes.len()) {
                    return Err(refuse(
                        container,
                        format!(
                            "/etc/resolv.conf is {} bytes, past the {limit}-byte file-size \
                             limit (RLIMIT_FSIZE) outrig is running under, so it could not \
                             be written back",
                            bytes.len()
                        ),
                    ));
                }
                Some(bytes)
            }
        };
        tracing::debug!(
            target: "outrig::network",
            container,
            present = file.is_some(),
            "opened the container's /etc/resolv.conf"
        );
        Ok(Self {
            place,
            file,
            original,
        })
    }

    /// The undo for pointing this resolver at `installed`, ready to
    /// [`install`](Restore::install): over the file [`open`](Self::open)
    /// found, or -- for a container that had none -- one created now.
    ///
    /// Creating is a change made before its undo exists, because the undo
    /// needs the descriptor creating hands back. The one gap that leaves is a
    /// child that made the file and whose descriptor never arrived, which
    /// [`NotCreated::left`] reports.
    pub(super) fn stage(self, installed: Vec<u8>) -> std::result::Result<Restore, Box<NotCreated>> {
        let file = match self.file {
            Some(file) => file,
            None => create_at(&self.place)?,
        };
        Ok(Restore {
            file,
            place: self.place,
            original: self.original,
            installed,
            unfinished: false,
        })
    }
}

/// The attach refusal every resolver this cannot put back gets.
fn refuse(container: &str, why: impl Display) -> OutrigError {
    OutrigError::Configuration(format!(
        "container {container:?} cannot be network-intercepted: {why}"
    ))
}

/// Creates the resolver a container did not have, open read-write.
fn create_at(place: &Place) -> std::result::Result<File, Box<NotCreated>> {
    let not_created = |source: io::Error, maybe_left: bool| {
        Box::new(NotCreated {
            left: maybe_left.then(|| {
                OutrigError::Configuration(format!(
                    "/etc/resolv.conf may have been created by this attach and lost hold of, \
                 so nothing can tell it from one this attach did not create: {source}"
                ))
            }),
            error: OutrigError::Path {
                op: "create",
                path: path().to_path_buf(),
                source,
            },
        })
    };
    // The child never answered, so whether it got as far as the `open` is
    // unknown.
    let (status, fds) = run_in(place, create).map_err(|e| not_created(e, true))?;
    if status != Status::OK {
        return Err(not_created(step_failure(status), false));
    }
    fds.into_iter().next().map(File::from).ok_or_else(|| {
        not_created(
            io::Error::new(
                io::ErrorKind::InvalidData,
                "the helper reported creating it and returned no descriptor",
            ),
            true,
        )
    })
}

/// [`Resolver::stage`] failing to create a resolver.
#[derive(Debug)]
pub(super) struct NotCreated {
    /// Why the attach fails.
    pub(super) error: OutrigError,
    /// What the container may have been left holding: a resolver that nothing
    /// can identify as this attach's, and so nothing can take back.
    pub(super) left: Option<OutrigError>,
}

/// Replaces the whole of `file` with `bytes`. Truncates first, so a reader
/// sees an empty file rather than new bytes over an old tail.
///
/// Refuses, before touching the file, bytes that would take it past the
/// file-size limit -- which would not fail the write but end this process.
fn write_whole(file: &File, bytes: &[u8]) -> io::Result<()> {
    if let Some(limit) = past_file_size_limit(bytes.len()) {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!(
                "{} bytes is past the {limit}-byte file-size limit (RLIMIT_FSIZE) outrig \
                 is running under, and writing them would end it with SIGXFSZ",
                bytes.len()
            ),
        ));
    }
    file.set_len(0)?;
    file.write_all_at(bytes, 0)
}

/// The file-size limit this process runs under, when a file of `len` bytes
/// would pass it.
///
/// A write past `RLIMIT_FSIZE` does not fail: the kernel sends `SIGXFSZ`,
/// whose default action ends the whole process, and nothing in outrig handles
/// it. When the writer was a shell in the container that cost only the shell;
/// in-process it would end outrig, with the resolver half written. So such a
/// write is never issued. The limit is inherited from whoever started outrig,
/// and is asked at each write rather than once because another process of the
/// same user can lower it.
fn past_file_size_limit(len: usize) -> Option<u64> {
    let mut limit = MaybeUninit::<libc::rlimit>::uninit();
    // Only an invalid resource or pointer fails this, and neither is possible
    // here; were it to, there is no limit to report.
    if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, limit.as_mut_ptr()) } == -1 {
        return None;
    }
    let soft = unsafe { limit.assume_init() }.rlim_cur;
    if soft == libc::RLIM_INFINITY {
        return None;
    }
    #[allow(clippy::unnecessary_cast)] // `rlim_t` is not `u64` on every libc
    let soft = soft as u64;
    (len as u64 > soft).then_some(soft)
}

/// The undo for an install: puts back what attach found, but only while the
/// resolver still holds what the install wrote.
///
/// The guard keeps a later legitimate change. A resolver manager that pointed
/// the container somewhere else after interception was installed has done
/// something this must not discard, and the comparison is of every byte, so a
/// newline or a NUL added or dropped counts as a change. The installed text
/// carries the attach's own marker, so another attachment's install never
/// compares equal either -- though a held descriptor could not reach another
/// container's file anyway.
#[derive(Debug)]
pub(super) struct Restore {
    file: File,
    place: Place,
    /// What attach found: the file's bytes, or `None` for no file at all.
    original: Option<Vec<u8>>,
    installed: Vec<u8>,
    /// Set while the file may hold this attach's own partial work: from the
    /// start of any write until the install it belongs to has finished. The
    /// guard would read partial work as "not ours" and retire the undo over a
    /// resolver left empty or half written, so while this is set the undo
    /// writes without asking. A finished install clears it, which is what
    /// keeps a later legitimate change protected.
    unfinished: bool,
}

impl Restore {
    /// A restore acting through `file`, however a test opened it.
    #[cfg(test)]
    pub(super) fn new(
        file: File,
        place: Place,
        original: Option<Vec<u8>>,
        installed: Vec<u8>,
    ) -> Self {
        Self {
            file,
            place,
            original,
            installed,
            unfinished: false,
        }
    }

    /// Points the resolver at what this was staged with: the change this
    /// undoes.
    pub(super) fn install(&mut self) -> Result<()> {
        self.unfinished = true;
        write_whole(&self.file, &self.installed).path_ctx("write", path())?;
        self.unfinished = false;
        Ok(())
    }

    /// Puts the resolver back. `Ok` also when there is nothing to put back,
    /// because the resolver is no longer what this installed; an error means
    /// the undo is still owed.
    ///
    /// A read that fails is not a mismatch. Reading it as one would retire
    /// the undo and let `detach` report success over a resolver still pointing
    /// at a listener that has stopped.
    pub(super) fn apply(&mut self) -> Result<()> {
        self.put_back().path_ctx("restore", path())
    }

    fn put_back(&mut self) -> io::Result<()> {
        if !self.unfinished {
            let current = read_at_most(&self.file, self.installed.len() + 1)?;
            if current != self.installed {
                tracing::debug!(
                    target: "outrig::network",
                    "the container's /etc/resolv.conf changed after interception; left as it is"
                );
                return Ok(());
            }
        }
        match &self.original {
            Some(original) => {
                self.unfinished = true;
                write_whole(&self.file, original)?;
            }
            None => self.remove()?,
        }
        tracing::debug!(target: "outrig::network", "restored the container's /etc/resolv.conf");
        Ok(())
    }

    /// The inverse of a resolver that was not there: remove the file this
    /// attach created, and only that file.
    ///
    /// By path, which is the one thing a descriptor cannot do, and so checked
    /// against the descriptor: the child removes what is at the path only when
    /// it is the inode this holds. A path the container has since removed, or
    /// pointed at a file of its own, owes nothing.
    fn remove(&self) -> io::Result<()> {
        let held = self.file.metadata()?;
        let (dev, ino) = (held.dev(), held.ino());
        let (status, _fds) = run_in(&self.place, |sock, path| unlink_if(sock, path, dev, ino))?;
        if status != Status::OK {
            return Err(step_failure(status));
        }
        Ok(())
    }

    /// What this would put back, for a test to recognize it by.
    #[cfg(test)]
    pub(super) fn describe(&self) -> String {
        match &self.original {
            Some(original) => format!(
                "restore /etc/resolv.conf to {:?}",
                String::from_utf8_lossy(original)
            ),
            None => "remove /etc/resolv.conf".to_string(),
        }
    }
}

/// Reads up to `limit` bytes from the start of `file`, by position, so the
/// descriptor's offset plays no part.
fn read_at_most(file: &File, limit: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0; limit];
    let mut filled = 0;
    while filled < limit {
        match file.read_at(&mut buf[filled..], filled as u64) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    // A snapshot is held for as long as the attachment is, and the limit is
    // far past what a resolver needs.
    buf.shrink_to_fit();
    Ok(buf)
}

/// Runs `body` in a forked child at `place`: inside the container's user and
/// mount namespaces as its root, with `body` handed the resolver's path there.
fn run_in<F>(place: &Place, body: F) -> io::Result<(Status, Vec<OwnedFd>)>
where
    F: FnOnce(RawFd, &CStr),
{
    let collected = match place {
        Place::Container(pid) => {
            let ns = nsfork::UserMountNs::open(*pid).map_err(|(file, e)| {
                let which = match file {
                    NsFile::User => "user",
                    NsFile::Mount => "mount",
                };
                io::Error::new(
                    e.kind(),
                    format!("could not open the container's {which} namespace: {e}"),
                )
            })?;
            nsfork::fork_collect(|sock| match ns.enter() {
                Ok(()) => body(sock, RESOLV_CONF),
                Err((step, errno)) => reply_failure(sock, step.into(), errno),
            })
        }
        #[cfg(test)]
        Place::Host(path) => nsfork::fork_collect(|sock| body(sock, path)),
    };
    Ok(collected?)
}

/// Where a resolver helper stopped. The numbering is the wire format between
/// the child and its parent, so the values are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum Step {
    SetnsUser = 1,
    SetIds = 2,
    SetnsMount = 3,
    Lstat = 4,
    Stat = 5,
    Dangling = 6,
    NotRegular = 7,
    Open = 8,
    Fstat = 9,
    Create = 10,
    Unlink = 11,
}

impl Step {
    /// Every step, in discriminant order -- the one place the wire codes are
    /// listed, so [`Step::from_code`] cannot drift from the enum.
    const ALL: [Step; 11] = [
        Step::SetnsUser,
        Step::SetIds,
        Step::SetnsMount,
        Step::Lstat,
        Step::Stat,
        Step::Dangling,
        Step::NotRegular,
        Step::Open,
        Step::Fstat,
        Step::Create,
        Step::Unlink,
    ];

    fn from_code(code: u32) -> Option<Self> {
        let index = usize::try_from(code).ok()?.checked_sub(1)?;
        Step::ALL.get(index).copied()
    }

    fn label(self) -> &'static str {
        match self {
            Step::SetnsUser => "enter the container's user namespace",
            Step::SetIds => "become the container's root",
            Step::SetnsMount => "enter the container's mount namespace",
            Step::Lstat => "look up /etc/resolv.conf",
            Step::Stat => "follow /etc/resolv.conf",
            // These two are refusals, which `Resolver::open` words itself;
            // the labels are for completeness.
            Step::Dangling => "follow /etc/resolv.conf to a file",
            Step::NotRegular => "use /etc/resolv.conf as a regular file",
            Step::Open => "open /etc/resolv.conf",
            Step::Fstat => "inspect the opened /etc/resolv.conf",
            Step::Create => "create /etc/resolv.conf",
            Step::Unlink => "remove /etc/resolv.conf",
        }
    }
}

impl From<EnterStep> for Step {
    fn from(step: EnterStep) -> Self {
        match step {
            EnterStep::SetnsUser => Step::SetnsUser,
            EnterStep::SetIds => Step::SetIds,
            EnterStep::SetnsMount => Step::SetnsMount,
        }
    }
}

/// The failure a helper reported in `status`, keeping its errno's kind.
fn step_failure(status: Status) -> io::Error {
    let cause = io::Error::from_raw_os_error(status.errno);
    let step = Step::from_code(status.step).map_or("finish", Step::label);
    io::Error::new(cause.kind(), format!("could not {step}: {cause}"))
}

fn reply_ok(sock: RawFd, fds: &[RawFd]) {
    let _ = nsfork::send_status(sock, Status::OK, fds);
}

fn reply_failure(sock: RawFd, step: Step, errno: i32) {
    nsfork::send_failure(sock, step as u32, errno);
}

/// `lstat(2)`, with nothing at `path` as `None` and a failure as its errno.
/// Safe to call from a forked child.
fn lstat(path: &CStr) -> std::result::Result<Option<libc::stat>, i32> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::lstat(path.as_ptr(), st.as_mut_ptr()) } == -1 {
        let errno = nsfork::errno();
        return if errno == libc::ENOENT {
            Ok(None)
        } else {
            Err(errno)
        };
    }
    Ok(Some(unsafe { st.assume_init() }))
}

/// `stat(2)`, a failure as its errno. Safe to call from a forked child.
fn stat(path: &CStr) -> std::result::Result<libc::stat, i32> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::stat(path.as_ptr(), st.as_mut_ptr()) } == -1 {
        return Err(nsfork::errno());
    }
    Ok(unsafe { st.assume_init() })
}

/// `fstat(2)`, a failure as its errno. Safe to call from a forked child.
fn fstat(fd: RawFd) -> std::result::Result<libc::stat, i32> {
    let mut st = MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, st.as_mut_ptr()) } == -1 {
        return Err(nsfork::errno());
    }
    Ok(unsafe { st.assume_init() })
}

fn is_regular(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// Finds out what is at `path` and, for a regular file, opens it read-write
/// and replies with the descriptor. No descriptor and an OK status means there
/// is nothing there. Runs in the forked child: syscalls only.
///
/// The type is checked before the `open` as well as after it. After, because
/// the path can change between the two; before, because opening is not free
/// for everything -- a device can act on being opened.
fn probe(sock: RawFd, path: &CStr) {
    match lstat(path) {
        Ok(Some(_)) => {}
        Ok(None) => return reply_ok(sock, &[]),
        Err(errno) => return reply_failure(sock, Step::Lstat, errno),
    }
    match stat(path) {
        Ok(st) if is_regular(&st) => {}
        Ok(_) => return reply_failure(sock, Step::NotRegular, libc::EINVAL),
        // Something is there, and following it finds nothing: a link to a
        // file that does not exist.
        Err(libc::ENOENT) => return reply_failure(sock, Step::Dangling, libc::ENOENT),
        Err(errno) => return reply_failure(sock, Step::Stat, errno),
    }
    let flags = libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(path.as_ptr(), flags) };
    if fd == -1 {
        return reply_failure(sock, Step::Open, nsfork::errno());
    }
    match fstat(fd) {
        Ok(st) if is_regular(&st) => reply_ok(sock, &[fd]),
        Ok(_) => {
            nsfork::close_fd(fd);
            reply_failure(sock, Step::NotRegular, libc::EINVAL);
        }
        Err(errno) => {
            nsfork::close_fd(fd);
            reply_failure(sock, Step::Fstat, errno);
        }
    }
}

/// Creates the resolver a container did not have and replies with it open
/// read-write. Exclusive, so a file that appeared since the probe -- or a link
/// planted there -- is a failure rather than something written through. Runs
/// in the forked child: syscalls only.
fn create(sock: RawFd, path: &CStr) {
    let flags = libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC;
    let fd = unsafe { libc::open(path.as_ptr(), flags, 0o644 as libc::c_uint) };
    if fd == -1 {
        return reply_failure(sock, Step::Create, nsfork::errno());
    }
    if nsfork::send_status(sock, Status::OK, &[fd]).is_err() {
        // Nobody will ever hold it, so nobody could take it back.
        unsafe { libc::unlink(path.as_ptr()) };
    }
}

/// Removes what is at `path` when it is the inode `dev`/`ino`. Anything else
/// there -- or nothing -- is not this attach's to remove, and is a success.
/// Runs in the forked child: syscalls only.
fn unlink_if(sock: RawFd, path: &CStr, dev: u64, ino: u64) {
    let st = match lstat(path) {
        Ok(Some(st)) => st,
        Ok(None) => return reply_ok(sock, &[]),
        Err(errno) => return reply_failure(sock, Step::Lstat, errno),
    };
    #[allow(clippy::unnecessary_cast)] // `dev_t` and `ino_t` are not `u64` on every libc
    let ours = st.st_dev as u64 == dev && st.st_ino as u64 == ino;
    if ours && unsafe { libc::unlink(path.as_ptr()) } == -1 {
        let errno = nsfork::errno();
        if errno != libc::ENOENT {
            return reply_failure(sock, Step::Unlink, errno);
        }
    }
    reply_ok(sock, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What an install writes, for the attach these tests stand in for.
    const INSTALLED: &[u8] = b"nameserver 127.0.0.1\noptions ndots:0\n# outrig_test\n";

    fn open(path: &Path) -> Result<Resolver> {
        Resolver::open(Place::host(path), "outrig-test")
    }

    /// Opens `path` the way attach opens a container's resolver and installs
    /// [`INSTALLED`] -- returning the undo attach would arm.
    fn attach(path: &Path) -> Restore {
        let mut restore = open(path)
            .expect("open")
            .stage(INSTALLED.to_vec())
            .expect("stage");
        restore.install().expect("install");
        restore
    }

    #[test]
    fn step_codes_round_trip() {
        for (code, step) in (1u32..).zip(Step::ALL) {
            assert_eq!(step as u32, code);
            assert_eq!(Step::from_code(code), Some(step));
        }
        assert!(Step::from_code(0).is_none());
        assert!(Step::from_code(Step::ALL.len() as u32 + 1).is_none());
    }

    /// The bytes go back exactly as they were. They never pass through a
    /// shell or an argument, so nothing about them -- quotes, `%`, a NUL -- is
    /// anything but data.
    #[test]
    fn a_restore_reproduces_arbitrary_bytes() {
        const NASTY: &[u8] =
            b"nameserver 10.0.0.1 # ' \"$(touch pwned)\" `id` \\ '' \n%s%d\noptions x\0tail";
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, NASTY).expect("write");

        let mut restore = attach(&path);
        assert_eq!(std::fs::read(&path).expect("installed"), INSTALLED);
        restore.apply().expect("restore");

        assert_eq!(std::fs::read(&path).expect("restored"), NASTY);
    }

    /// A resolver something has legitimately changed since is no longer the
    /// one this attach installed, and taking it back would discard that
    /// change -- as would taking back one another attachment installed. Every
    /// byte is compared, so a newline or a NUL added or dropped is a change.
    #[test]
    fn a_restore_leaves_a_resolver_it_did_not_install_alone() {
        let mut nul_added = INSTALLED.to_vec();
        nul_added.insert(INSTALLED.len() - 1, 0);
        let changes: [Vec<u8>; 6] = [
            // Pointed somewhere else, marker untouched.
            String::from_utf8_lossy(INSTALLED)
                .replace("127.0.0.1", "9.9.9.9")
                .into_bytes(),
            // An option added, marker untouched.
            [INSTALLED, b"search added.test\n"].concat(),
            // What a different attachment installed.
            String::from_utf8_lossy(INSTALLED)
                .replace("outrig_test", "outrig_other")
                .into_bytes(),
            // A trailing newline gained, and one lost.
            [INSTALLED, b"\n"].concat(),
            INSTALLED[..INSTALLED.len() - 1].to_vec(),
            nul_added,
        ];
        for changed in changes {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("resolv.conf");
            std::fs::write(&path, b"nameserver 10.0.2.3\n").expect("write");
            let mut restore = attach(&path);

            std::fs::write(&path, &changed).expect("change");
            restore
                .apply()
                .expect("the guard makes this a no-op, not a failure");

            assert_eq!(
                std::fs::read(&path).expect("untouched"),
                changed,
                "an undo must not discard a change it did not make: {:?}",
                String::from_utf8_lossy(&changed)
            );
        }
    }

    /// A resolver that cannot be read is not a resolver that belongs to
    /// someone else. Reading the failure as a mismatch would retire the undo
    /// while `detach` reported success over a resolver still pointing at a
    /// listener that has stopped.
    #[test]
    fn an_unreadable_resolver_fails_the_undo_rather_than_skipping_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, INSTALLED).expect("write");
        let write_only = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open write-only");
        let mut restore = Restore::new(
            write_only,
            Place::host(&path),
            Some(b"nameserver 10.0.2.3\n".to_vec()),
            INSTALLED.to_vec(),
        );

        restore
            .apply()
            .expect_err("an unreadable resolver has to fail the undo, not retire it");
        assert_eq!(std::fs::read(&path).expect("untouched"), INSTALLED);
    }

    /// A file that can be truncated and not written: a write to it fails
    /// after its truncate has already emptied the file, which is the partial
    /// write an undo has to survive.
    fn truncatable_but_not_writable(content: &[u8]) -> File {
        use std::os::fd::FromRawFd as _;

        let flags = libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC;
        let fd = unsafe { libc::memfd_create(c"resolv.conf".as_ptr(), flags) };
        assert_ne!(fd, -1, "memfd_create: {}", io::Error::last_os_error());
        let file = unsafe { File::from_raw_fd(fd) };
        file.write_all_at(content, 0).expect("fill");
        let sealed = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_WRITE) };
        assert_ne!(sealed, -1, "seal: {}", io::Error::last_os_error());
        file
    }

    /// An install that fails after its truncate leaves the resolver empty or
    /// half written. That is this attach's own work, so the undo stays owed
    /// rather than reading it as a change someone else made and retiring.
    #[test]
    fn an_install_that_failed_partway_stays_owed() {
        const ORIGINAL: &[u8] = b"nameserver 10.0.2.3\n";
        let mut restore = Restore::new(
            truncatable_but_not_writable(ORIGINAL),
            Place::host(Path::new("/unused")),
            Some(ORIGINAL.to_vec()),
            INSTALLED.to_vec(),
        );
        restore.install().expect_err("the write is refused");
        assert_eq!(restore.file.metadata().expect("stat").len(), 0, "truncated");

        restore
            .apply()
            .expect_err("still owed: the write back is attempted, not skipped");
    }

    /// The same for a restore that fails after its truncate: the retry finds
    /// the restore's own partial work, and writes again rather than retiring.
    #[test]
    fn a_restore_that_failed_partway_stays_owed() {
        let mut restore = Restore::new(
            truncatable_but_not_writable(INSTALLED),
            Place::host(Path::new("/unused")),
            Some(b"nameserver 10.0.2.3\n".to_vec()),
            INSTALLED.to_vec(),
        );
        restore.apply().expect_err("the write is refused");
        restore
            .apply()
            .expect_err("still owed: the retry writes, not retires");
    }

    /// What an undo left unfinished does with a file it can write: puts the
    /// original back over whatever partial work it finds there.
    #[test]
    fn an_unfinished_undo_writes_over_its_partial_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, b"nameserver 10.0.2.3\n").expect("write");
        let mut restore = attach(&path);
        assert!(
            !restore.unfinished,
            "a finished install leaves the guard on"
        );

        std::fs::write(&path, &INSTALLED[..10]).expect("half written");
        restore.unfinished = true;
        restore.apply().expect("restore");
        assert_eq!(
            std::fs::read(&path).expect("restored"),
            b"nameserver 10.0.2.3\n"
        );
    }

    /// Past `RLIMIT_FSIZE` a write ends the process with `SIGXFSZ`, so a
    /// resolver this could not write back is refused at attach, and a restore
    /// that finds the limit lowered since refuses the write rather than
    /// issuing it. Run in a child holding the limit, since getting this wrong
    /// takes down the process that does it.
    #[test]
    fn a_file_size_limit_refuses_the_write_rather_than_ending_outrig() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};

        const LIMIT: usize = 16 * 1024;
        const CHILD: &str = "OUTRIG_TEST_FILE_SIZE_LIMIT_DIR";
        if let Some(dir) = std::env::var_os(CHILD) {
            let dir = Path::new(&dir);
            let installed = dir.join("installed");
            let read_write = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&installed)
                .expect("open read-write");
            let mut restore = Restore::new(
                read_write,
                Place::host(&installed),
                Some(vec![b'#'; 2 * LIMIT]),
                INSTALLED.to_vec(),
            );
            let err = restore.apply().expect_err("refused at restore");
            assert!(err.to_string().contains("SIGXFSZ"), "{err}");
            assert_eq!(
                std::fs::read(&installed).expect("read"),
                INSTALLED,
                "refused before the file was touched"
            );

            let err = open(&dir.join("big")).expect_err("refused at attach");
            assert!(err.to_string().contains("RLIMIT_FSIZE"), "{err}");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("big"), vec![b'#'; 2 * LIMIT]).expect("write");
        std::fs::write(dir.path().join("installed"), INSTALLED).expect("write");
        let (_crate, module) = module_path!().split_once("::").expect("crate-qualified");
        let name =
            format!("{module}::a_file_size_limit_refuses_the_write_rather_than_ending_outrig");
        let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"));
        child.args(["--exact", &name]).env(CHILD, dir.path());
        unsafe {
            child.pre_exec(|| {
                let limit = libc::rlimit {
                    rlim_cur: LIMIT as libc::rlim_t,
                    rlim_max: LIMIT as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) == -1 {
                    return Err(io::Error::last_os_error());
                }
                libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
                Ok(())
            });
        }
        let out = child.output().expect("run the test binary");
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("1 passed"),
            "the child holding the limit failed ({:?}; SIGXFSZ is {}): {}{}",
            out.status.signal(),
            libc::SIGXFSZ,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A resolver the container removed after the install is not this
    /// attach's to bring back.
    #[test]
    fn a_resolver_the_container_removed_is_not_recreated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, b"nameserver 10.0.2.3\n").expect("write");
        let mut restore = attach(&path);

        std::fs::remove_file(&path).expect("remove");
        restore.apply().expect("nothing is owed");

        assert!(!path.exists(), "the restore must not recreate the path");
    }

    /// Having no resolver file at all is a state too: the restore removes the
    /// file the install created rather than leaving an empty one behind.
    #[test]
    fn a_container_with_no_resolver_file_is_restored_to_having_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");

        let mut restore = attach(&path);
        assert_eq!(std::fs::read(&path).expect("created"), INSTALLED);
        restore.apply().expect("restore");

        assert!(!path.exists(), "the created resolver must be removed");
    }

    /// The removal is by path, so it is checked against the file this created:
    /// a resolver put at that path since, even one with the same bytes, is
    /// not this attach's to remove.
    #[test]
    fn a_created_resolver_replaced_since_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        let mut restore = attach(&path);

        let replacement = dir.path().join("replacement");
        std::fs::write(&replacement, INSTALLED).expect("write");
        std::fs::rename(&replacement, &path).expect("replace");
        restore.apply().expect("nothing is owed");

        assert_eq!(std::fs::read(&path).expect("left alone"), INSTALLED);
    }

    /// Creating is exclusive, so a resolver that appeared after the probe
    /// said there was none is not written through.
    #[test]
    fn a_resolver_that_appears_after_the_probe_is_not_written_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        let resolver = open(&path).expect("open");
        std::fs::write(&path, b"nameserver 10.0.2.3\n").expect("appear");

        let err = resolver
            .stage(INSTALLED.to_vec())
            .expect_err("an exclusive create cannot succeed over a file");
        assert!(
            err.error.to_string().contains("could not create"),
            "{}",
            err.error
        );
        assert!(err.left.is_none(), "a refused create made nothing");
        assert_eq!(
            std::fs::read(&path).expect("untouched"),
            b"nameserver 10.0.2.3\n"
        );
    }

    /// A resolver that is a link to nothing. Installing would follow the link
    /// and create its target, and the undo for an absent resolver removes the
    /// path -- which would take the link and leave the file. Refused before
    /// anything is written.
    #[test]
    fn a_dangling_resolver_link_refuses_the_attach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &path).expect("symlink");

        let err = open(&path).expect_err("a dangling link cannot be put back");
        assert!(err.to_string().contains("symbolic link"), "{err}");
        assert!(!dir.path().join("nowhere").exists());
    }

    /// Anything but a regular file is refused without being opened: reading a
    /// FIFO blocks for as long as nothing writes it, so a container could
    /// otherwise hold an attach open forever.
    #[test]
    fn a_resolver_that_is_not_a_regular_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fifo = dir.path().join("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).expect("mkfifo");
        let directory = dir.path().join("directory");
        std::fs::create_dir(&directory).expect("mkdir");

        for path in [fifo, directory] {
            let err = open(&path).expect_err("only a regular file can be put back");
            assert!(err.to_string().contains("not a regular file"), "{err}");
        }
    }

    /// The snapshot is held in memory until the undo runs, so how much of one
    /// a container can make outrig hold is bounded.
    #[test]
    fn an_oversized_resolver_refuses_the_attach() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("resolv.conf");
        std::fs::write(&path, vec![b'x'; MAX_RESOLV_SNAPSHOT + 1]).expect("write");

        let err = open(&path).expect_err("an oversized resolver is not one to hold");
        assert!(err.to_string().contains("bytes"), "{err}");
    }
}
