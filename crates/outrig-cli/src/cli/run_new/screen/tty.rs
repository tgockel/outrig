//! The terminal in raw mode, and every way back out of it.
//!
//! The screen reads keys one at a time and draws where it likes, so the
//! terminal is in raw mode for as long as it is up, and must be put back
//! however the screen ends. The way out it chooses is [`Raw::leave`], through
//! crossterm, which keeps raw mode's state in a static of its own that its key
//! decoding reads, so it must not be bypassed there. The others cannot take
//! that way:
//!
//! - **SIGTERM and SIGHUP** end the process by their default action, as they
//!   do without the screen; a handler installed while the screen is up first
//!   puts the terminal back, then lets the signal do what it would have done.
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
/// Set once a panic restored the terminal: the screen must not draw again.
static DEAD: AtomicBool = AtomicBool::new(false);
/// Set while the detail view holds the terminal's alternate screen.
static DETAIL: AtomicBool = AtomicBool::new(false);
/// The viewport's top row, or `u16::MAX` before there is one.
static TOP: AtomicU16 = AtomicU16::new(u16::MAX);
/// [`restore`]'s own descriptor for the terminal, or -1.
static RESTORE_FD: AtomicI32 = AtomicI32::new(-1);
/// The terminal's modes before raw mode.
static SAVED: Saved<libc::termios> = Saved::new();

/// The signals that end the process with the terminal in whatever state it
/// is in, and what each was set to do before the screen's handler.
const SIGNALS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];
static PREVIOUS: [Saved<libc::sigaction>; 2] = [Saved::new(), Saved::new()];

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
        terminal::enable_raw_mode()?;
        // From here an error drops `raw`, which puts the terminal back.
        let raw = Raw { active: true };
        ARMED.store(true, Ordering::SeqCst);
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

/// Whether a panic took the terminal back from the screen.
pub(super) fn dead() -> bool {
    DEAD.load(Ordering::SeqCst)
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
    restore();
    DEAD.store(true, Ordering::SeqCst);
}

/// Put the terminal back: out of the alternate screen, with no scroll region,
/// the viewport cleared, bracketed paste off, the cursor shown, and the modes
/// the shell gave. Once; safe in a signal handler.
fn restore() {
    if !ARMED.swap(false, Ordering::SeqCst) {
        return;
    }
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
    for (signal, previous) in SIGNALS.iter().zip(&PREVIOUS) {
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
            libc::sigaction(*signal, &ours, std::ptr::null_mut());
        }
    }
}

fn uninstall_handlers() {
    for (signal, previous) in SIGNALS.iter().zip(&PREVIOUS) {
        if let Some(previous) = previous.take() {
            // SAFETY: as in `install_handlers`.
            unsafe { libc::sigaction(*signal, &previous, std::ptr::null_mut()) };
        }
    }
}

extern "C" fn on_signal(signal: libc::c_int) {
    restore();
    let previous = SIGNALS
        .iter()
        .position(|&ours| ours == signal)
        .and_then(|index| PREVIOUS[index].get());
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

    fn take(&self) -> Option<T> {
        let value = self.get();
        self.set.store(false, Ordering::SeqCst);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
