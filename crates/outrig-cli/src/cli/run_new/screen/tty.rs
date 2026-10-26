//! The terminal in raw mode, and every way back out of it.
//!
//! The screen reads keys one at a time and draws where it likes, so the
//! terminal is in raw mode for as long as it is up, and must be put back
//! however the screen ends. The way out it chooses is [`Raw::leave`], through
//! crossterm, which keeps raw mode's state in a static of its own that its key
//! decoding reads, so it must not be bypassed there. The others cannot take
//! that way:
//!
//! - **SIGTERM and SIGHUP** go on to do what they would have done without the
//!   screen; a handler installed while the screen is up first puts the
//!   terminal back, then delivers the signal to whatever was there before --
//!   the session's own listener, which ends the session through its teardown,
//!   or the default action, which ends the process. Either way the terminal is
//!   the shell's again before anything else happens, and the screen is marked
//!   dead, so a session that goes on to stop does not draw over the terminal
//!   it has just given back. The screen's thread may be in the middle of a
//!   frame when that lands, so it repairs the terminal once more itself, after
//!   its last write ([`repair`]).
//! - **A panic** anywhere, through a hook chained ahead of the process's own,
//!   which also marks the screen dead so its thread stops drawing.
//! - **A screen that cannot be waited for**, stuck writing to a terminal that
//!   stopped reading.
//!
//! Those three run [`restore`], which is safe in a signal handler: it writes
//! fixed bytes with `write(2)`, to a descriptor of its own that does not
//! block, and sets the terminal's modes back with `tcsetattr`, from a copy
//! saved before raw mode was entered.

use std::cell::UnsafeCell;
use std::fs::File;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU16, Ordering};

use nix::libc;
use ratatui::crossterm::cursor::Show;
use ratatui::crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use ratatui::crossterm::{queue, terminal};

/// Set while the terminal is raw and [`restore`] has something to do.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Set once the terminal was taken back from the screen -- by a panic, by a
/// signal, or from outside: the screen must not draw again.
static DEAD: AtomicBool = AtomicBool::new(false);
/// Set once a signal took the terminal back, which is not a screen failure.
static SIGNALLED: AtomicBool = AtomicBool::new(false);
/// Set while the detail view holds the terminal's alternate screen.
static DETAIL: AtomicBool = AtomicBool::new(false);
/// The viewport's top row, or `u16::MAX` before there is one.
static TOP: AtomicU16 = AtomicU16::new(u16::MAX);
/// [`restore`]'s own descriptor for the terminal, or -1.
static RESTORE_FD: AtomicI32 = AtomicI32::new(-1);
/// The terminal's modes before raw mode.
static SAVED: Saved<libc::termios> = Saved::new();

/// The signals that end the process with the terminal in whatever state it
/// is in, and what each was set to do before the screen's handler. A
/// predecessor stays where it is once the handler is taken out: a signal that
/// is already inside the handler, on another thread, still has to be handed
/// to it, and would otherwise find it gone.
const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];
static PREVIOUS: [Saved<libc::sigaction>; 2] = [Saved::new(), Saved::new()];
/// Whether the screen's handler is the one installed for each.
static INSTALLED: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

/// The terminal in raw mode, until [`Raw::leave`] or drop.
pub(super) struct Raw {
    active: bool,
}

impl Raw {
    /// Put the terminal in raw mode, with bracketed paste on, writing to it
    /// through `out`.
    pub(super) fn enter(out: &mut impl Write) -> io::Result<Raw> {
        if SAVED.get().is_none() {
            let saved = nix::sys::termios::tcgetattr(io::stdin())?;
            // SAFETY: set once, before anything that reads it is armed.
            unsafe { SAVED.set(saved.into()) };
        }
        if RESTORE_FD.load(Ordering::SeqCst) < 0 {
            let tty = File::options()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open("/dev/tty")?;
            // Kept for the life of the process, which a signal can end at
            // any moment.
            RESTORE_FD.store(tty.into_raw_fd(), Ordering::SeqCst);
        }
        hook_panics();
        if dead() {
            return Err(given_up());
        }
        terminal::enable_raw_mode()?;
        // From here an error drops `raw`, which puts the terminal back.
        let raw = Raw { active: true };
        ARMED.store(true, Ordering::SeqCst);
        // Checked once armed too: either `abandon`'s `restore` saw it armed, or
        // this sees the screen dead, and backs out before any write that could
        // wait on a terminal that has stopped reading.
        if dead() {
            return Err(given_up());
        }
        install_handlers();
        queue!(out, EnableBracketedPaste)?;
        out.flush()?;
        Ok(raw)
    }

    /// Put the terminal back as the shell gave it, through `out`.
    pub(super) fn leave(&mut self, out: &mut impl Write) -> io::Result<()> {
        if !std::mem::take(&mut self.active) {
            return Ok(());
        }
        let written = queue!(out, DisableBracketedPaste, Show).and_then(|()| out.flush());
        let left = terminal::disable_raw_mode();
        ARMED.store(false, Ordering::SeqCst);
        uninstall_handlers();
        written.and(left)
    }

    /// Ctrl-Z: give the terminal back, stop the process as the terminal's
    /// own Ctrl-Z would have, and take the terminal again once continued.
    pub(super) fn suspend(&mut self, out: &mut impl Write) -> io::Result<()> {
        self.leave(out)?;
        // The whole foreground process group, as the terminal stops it.
        // SAFETY: `kill` takes no pointers.
        unsafe { libc::kill(0, libc::SIGTSTP) };
        *self = Raw::enter(out)?;
        Ok(())
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        if std::mem::take(&mut self.active) {
            restore();
            uninstall_handlers();
            // crossterm's own idea of the mode, which `restore` did not
            // change, set back to match.
            let _ = terminal::disable_raw_mode();
        }
    }
}

/// The terminal's file, as the screen writes it: nothing reaches the terminal
/// once it was taken back. A start given up on is still inside
/// [`Viewport::open`](super::viewport::Viewport::open) -- waiting for the
/// cursor's position, say -- and the moves and clears it goes on to would land
/// on whatever the session has written below the conversation by then, which
/// [`repair`] cannot give back.
pub(super) struct Held(File);

impl Held {
    pub(super) fn new(tty: File) -> Self {
        Self(tty)
    }
}

impl Write for Held {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if dead() {
            return Err(given_up());
        }
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

fn given_up() -> io::Error {
    io::Error::other("the screen was given up on before it had the terminal")
}

/// Whether the terminal was taken back from the screen, so it must not draw.
pub(super) fn dead() -> bool {
    DEAD.load(Ordering::SeqCst)
}

/// Whether it was a signal that took it back. Then the screen stopped because
/// the session is ending, which is not a failure to report.
pub(super) fn signalled() -> bool {
    SIGNALLED.load(Ordering::SeqCst)
}

/// The viewport moved to `top`: where [`restore`] clears from.
pub(super) fn viewport_at(top: u16) {
    TOP.store(top, Ordering::SeqCst);
}

/// The detail view opened or closed.
pub(super) fn detail_open(open: bool) {
    DETAIL.store(open, Ordering::SeqCst);
}

/// Take the terminal back from a screen that cannot be waited for.
pub(super) fn abandon() {
    // Published first: a thread still taking the terminal sees it, and backs
    // out, or arms restoration in time for the `restore` below to find it.
    DEAD.store(true, Ordering::SeqCst);
    restore();
}

/// Put the terminal back once more after something took it from the screen --
/// a signal, or a start given up on -- from the screen's own thread once it has
/// written for the last time. That restoration runs wherever the thread was: a
/// frame it was in the middle of, or the escapes of a start, land after, and
/// can leave the viewport drawn over what the session says as it closes, the
/// cursor hidden, or bracketed paste on. Nothing for a screen that ended on its
/// own, which the thread leaves as it should be.
pub(super) fn repair() {
    if dead() {
        put_back();
    }
}

/// Put the terminal back: out of the alternate screen, with no scroll region,
/// the viewport cleared, bracketed paste off, the cursor shown, and the modes
/// the shell gave. Once; safe in a signal handler.
fn restore() {
    if !ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
    put_back();
}

/// What [`restore`] does, as many times as it is asked to.
fn put_back() {
    let fd = RESTORE_FD.load(Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    if DETAIL.load(Ordering::SeqCst) {
        write_all(fd, b"\x1b[?1049l");
    }
    // Resetting the scroll region moves the cursor home; put it back.
    write_all(fd, b"\x1b7\x1b[r\x1b8");
    let top = TOP.load(Ordering::SeqCst);
    if top != u16::MAX {
        let mut at = [0u8; 16];
        let len = move_to_row(&mut at, top);
        write_all(fd, &at[..len]);
        write_all(fd, b"\x1b[J");
    }
    write_all(fd, b"\x1b[?2004l\x1b[?25h");
    if let Some(saved) = SAVED.get() {
        // SAFETY: `saved` is a valid termios, read from this terminal.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
}

/// `bytes` to `fd`, giving up at the first write that does not take any.
fn write_all(fd: libc::c_int, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        // SAFETY: the pointer and length describe `bytes`.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        if written <= 0 {
            return;
        }
        bytes = &bytes[written as usize..];
    }
}

/// The sequence that moves the cursor to the start of `row`, counted from 0,
/// written into `buf` without allocating. Returns its length.
fn move_to_row(buf: &mut [u8; 16], row: u16) -> usize {
    let mut digits = [0u8; 5];
    let mut count = 0;
    let mut n = u32::from(row) + 1;
    while n > 0 || count == 0 {
        digits[count] = b'0' + (n % 10) as u8;
        n /= 10;
        count += 1;
    }
    buf[..2].copy_from_slice(b"\x1b[");
    let mut len = 2;
    for digit in digits[..count].iter().rev() {
        buf[len] = *digit;
        len += 1;
    }
    buf[len..len + 3].copy_from_slice(b";1H");
    len + 3
}

fn hook_panics() {
    static HOOKED: Once = Once::new();
    HOOKED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if ARMED.load(Ordering::SeqCst) {
                restore();
                DEAD.store(true, Ordering::SeqCst);
            }
            previous(info);
        }));
    });
}

fn install_handlers() {
    for ((signal, previous), installed) in SIGNALS.iter().zip(&PREVIOUS).zip(&INSTALLED) {
        // SAFETY: `sigaction` reads and writes only the structs passed, which
        // are valid; the handler it installs is async-signal-safe.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(*signal, std::ptr::null(), &mut current) != 0
                || current.sa_sigaction == libc::SIG_IGN
            {
                continue;
            }
            previous.set(current);
            let mut ours: libc::sigaction = std::mem::zeroed();
            ours.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
            libc::sigemptyset(&mut ours.sa_mask);
            ours.sa_flags = libc::SA_RESTART;
            if libc::sigaction(*signal, &ours, std::ptr::null_mut()) == 0 {
                installed.store(true, Ordering::SeqCst);
            }
        }
    }
}

fn uninstall_handlers() {
    for ((signal, previous), installed) in SIGNALS.iter().zip(&PREVIOUS).zip(&INSTALLED) {
        if !installed.swap(false, Ordering::SeqCst) {
            continue;
        }
        if let Some(previous) = previous.get() {
            // SAFETY: as in `install_handlers`.
            unsafe { libc::sigaction(*signal, &previous, std::ptr::null_mut()) };
        }
    }
}

extern "C" fn on_signal(signal: libc::c_int) {
    // Read before anything is published: the screen's thread stops at `DEAD`
    // and drops `Raw`, which takes this handler out, and a signal that went
    // on to look for its predecessor after that -- preempted between the two
    // -- would find none and end the process by the default action, with the
    // session's container left running.
    let previous = SIGNALS
        .iter()
        .position(|&ours| ours == signal)
        .and_then(|index| PREVIOUS[index].get());
    restore();
    // The terminal is the shell's again, and the session may go on stopping
    // for a few seconds: nothing draws over it from here. `SIGNALLED` first,
    // so a thread that sees the screen dead can tell why.
    SIGNALLED.store(true, Ordering::SeqCst);
    DEAD.store(true, Ordering::SeqCst);
    // SAFETY: as in `install_handlers`; `raise` is async-signal-safe, and the
    // signal is delivered again once this handler returns, to whatever was
    // there before.
    unsafe {
        match previous {
            Some(previous) => {
                libc::sigaction(signal, &previous, std::ptr::null_mut());
            }
            None => {
                libc::signal(signal, libc::SIG_DFL);
            }
        }
        libc::raise(signal);
    }
}

/// A value written before a signal handler can read it, and read without a
/// lock, which a signal handler cannot take.
struct Saved<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    set: AtomicBool,
}

// SAFETY: the value is written only while `set` is clear, by `set`, whose
// callers guarantee nothing else writes at the same time; it is read only
// once `set` says it was written.
unsafe impl<T: Copy + Send> Sync for Saved<T> {}

impl<T: Copy> Saved<T> {
    const fn new() -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::uninit()),
            set: AtomicBool::new(false),
        }
    }

    /// # Safety
    ///
    /// Nothing else may call `set` at the same time.
    unsafe fn set(&self, value: T) {
        self.set.store(false, Ordering::SeqCst);
        // SAFETY: `set` is clear, so no reader looks at the value, and the
        // caller rules out another writer.
        unsafe { (*self.value.get()).write(value) };
        self.set.store(true, Ordering::SeqCst);
    }

    fn get(&self) -> Option<T> {
        // SAFETY: written before `set` was.
        self.set
            .load(Ordering::SeqCst)
            .then(|| unsafe { (*self.value.get()).assume_init() })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, PoisonError};

    use super::*;

    /// A signal already inside the screen's handler when the screen's thread
    /// takes it out -- `Raw` dropped as the screen ends -- still looks up what
    /// to hand the signal to. Taking the handler out must leave that where it
    /// is, or the signal finds none and ends the process by its default
    /// action, with the session's container left running (#644).
    #[test]
    fn taking_the_handler_out_leaves_its_predecessor_for_a_signal_already_in_it() {
        // The dispositions are the whole process's.
        static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
        let _only = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);

        install_handlers();
        if !INSTALLED[0].load(Ordering::SeqCst) {
            // Whoever started the tests ignores SIGTERM: nothing is installed
            // to take out.
            return;
        }
        let recorded = PREVIOUS[0].get().expect("the predecessor is recorded");

        uninstall_handlers();
        assert!(!INSTALLED[0].load(Ordering::SeqCst), "the handler is out");
        let kept = PREVIOUS[0]
            .get()
            .expect("the predecessor is still there for a signal already in the handler");
        assert_eq!(kept.sa_sigaction, recorded.sa_sigaction);

        // Taken out once, it is not taken out again: that would put back a
        // predecessor that something else has since replaced.
        uninstall_handlers();
    }

    #[test]
    fn the_row_sequence_counts_from_one() {
        let mut buf = [0u8; 16];
        let len = move_to_row(&mut buf, 0);
        assert_eq!(&buf[..len], b"\x1b[1;1H");
        let len = move_to_row(&mut buf, 41);
        assert_eq!(&buf[..len], b"\x1b[42;1H");
        let len = move_to_row(&mut buf, u16::MAX - 1);
        assert_eq!(&buf[..len], b"\x1b[65535;1H");
    }
}
