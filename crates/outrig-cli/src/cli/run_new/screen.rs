//! `run-new` on a terminal: the conversation in the terminal's own scrollback,
//! and a status line and an input box below it.
//!
//! What the user sends and what the agent sends them are the conversation,
//! and print as they happen, into history the terminal keeps after the session
//! ends. The agent's working -- the model's own text, each submission and what
//! it printed, retries -- is detail, which Ctrl-O shows in a view of its own
//! and `-v` prints in the conversation. The status line says what the session
//! is doing meanwhile, from its events.
//!
//! The screen is used only when stdin and stdout are both the controlling
//! terminal, and on a terminal rustyline would drive: see [`wanted`].
//! Otherwise `run-new` reads and writes plain lines, exactly as before.
//!
//! One thread owns the terminal: it draws, and is the only thing that writes
//! there while the screen is up. The conversation's loop hands it what to show
//! through a queue and never waits on it, as it never waits on
//! [`Output`](super::output::Output)'s threads. Log records are diverted to it
//! too, so a warning does not land on top of what it drew. The agent's
//! messages take a place of the [`OUTBOX`] the line mode has, held until each
//! is drawn, so the agent is held to the terminal's pace exactly as it is
//! there -- including while the detail view is open, which holds them back as
//! a paused pager would.
//!
//! Keys are read on a thread of their own, which owns the input box, so a
//! terminal that has stopped reading what is written to it, holding the
//! drawing thread up in a write, never keeps Ctrl-C or a typed `/quit` from
//! the loop.

#![deny(clippy::print_stdout, clippy::print_stderr)]

mod editor;
mod facts;
mod render;
mod status;
mod transcript;
mod tty;
mod viewport;

use std::collections::VecDeque;
use std::fs::File;
use std::future::Future;
use std::io::{self, BufWriter, IsTerminal};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use outrig::harness::event::Received;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use self::editor::{Edit, Editor};
use self::facts::Fact;
use self::render::Theme;
use self::status::Status;
use self::transcript::{Entry, Kind, Transcript};
use self::tty::{Held, Raw};
use self::viewport::{Panel, Viewport};
use super::output::{Failures, OUTBOX, same_destination};
use crate::cli::log_divert::{Diverted, LogWriter};
use crate::repl::{is_controlling_tty, tty_capable};

/// How long the thread waits for a key before it looks at its queue.
const TICK: Duration = Duration::from_millis(30);
/// How often the spinner turns, and a running clock is drawn again.
const SPIN: Duration = Duration::from_millis(100);
/// How long, once the conversation is over, the thread is given to show the
/// rest and put the terminal back, before it is done from outside.
const FINISH_GRACE: Duration = Duration::from_secs(2);

/// Lines typed, as they reach the conversation's loop.
pub(super) type Typed = mpsc::UnboundedReceiver<io::Result<String>>;

/// How long a start given up on is waited for, once the terminal is put back,
/// to be done with it: past that, it is stuck on a terminal that stopped
/// reading, and the session goes on without it.
const GIVE_UP_GRACE: Duration = Duration::from_millis(500);

/// The start was given up on, as a signal ended the wait for it: put the
/// terminal back, and have the thread stop at its next look. Then wait up to
/// [`GIVE_UP_GRACE`] for `start`, which the thread answers once its last write
/// is made, so that what the session says next is not met by one.
pub(super) async fn give_up(start: impl Future<Output = io::Result<Started>>) {
    tty::abandon();
    // The thread's answer: failed, with the terminal repaired, or opened before
    // it saw the terminal taken, and then asked to finish.
    if let Ok(Ok(mut started)) = tokio::time::timeout(GIVE_UP_GRACE, start).await {
        started.screen.finish().await;
    }
}

/// Whether a signal took the terminal back from the screen.
pub(super) fn signalled() -> bool {
    tty::signalled()
}

/// Whether `run-new` takes the terminal: stdin is the controlling terminal and
/// one rustyline would drive, and stdout is the same terminal. stdout must be,
/// as crossterm asks the terminal where its cursor is by writing to stdout;
/// so `run-new > out.txt` keeps the line mode, and its stdout keeps only what
/// the agent sends.
pub(super) fn wanted() -> bool {
    let stdin = io::stdin();
    decide(
        stdin.is_terminal(),
        is_controlling_tty(&stdin),
        std::env::var("TERM").ok().as_deref(),
        same_destination(&stdin, &io::stdout()),
    )
}

fn decide(stdin_tty: bool, controlling: bool, term: Option<&str>, stdout_too: bool) -> bool {
    tty_capable(stdin_tty, controlling, term) && stdout_too
}

/// The line the conversation shows of `received`, if it shows one: what the
/// line mode prints of the session's events, so its notes are the screen's.
pub(super) fn conversation_line(received: &Received) -> Option<String> {
    match facts::fact(received, &facts::Names::default())? {
        Fact::Note { text, inline: true } => Some(text),
        _ => None,
    }
}

/// What the conversation's loop hands the screen, in the order it happened.
enum ToScreen {
    /// An entry, and for a message the agent sent, its place in the outbox,
    /// held until it is drawn.
    Entry(Entry, Option<OwnedSemaphorePermit>),
    Fact(Fact),
    /// A round ended: what it said, and what it left running.
    Ended {
        entries: Vec<Entry>,
        still_running: Option<String>,
    },
    /// The channel took a typed line, with this many waiting.
    Queued(usize),
    /// Dropped once everything before it is drawn.
    Written(oneshot::Sender<()>),
    /// Show what is left, put the terminal back, and answer.
    Finish(oneshot::Sender<()>),
}

/// The screen, as the conversation's loop holds it.
pub(super) struct Screen {
    queue: mpsc::UnboundedSender<ToScreen>,
    outbox: Arc<Semaphore>,
    abandoned: AtomicBool,
    finished: bool,
    logs: Option<Diverted>,
    /// What the children's launches named them, for labeling their events.
    names: Mutex<facts::Names>,
}

/// A started screen, and what it hands the loop: the lines typed, each
/// Ctrl-C pressed, and the writes that failed.
pub(super) struct Started {
    pub(super) screen: Screen,
    pub(super) typed: Typed,
    pub(super) keys: mpsc::UnboundedReceiver<()>,
    pub(super) failures: Failures,
}

/// A place in the outbox, for a message the loop has yet to take.
pub(super) struct Room<'a> {
    place: OwnedSemaphorePermit,
    screen: &'a Screen,
}

impl Room<'_> {
    pub(super) fn send(self, text: String) {
        let entry = Entry::new(Kind::Agent, text);
        self.screen.say(ToScreen::Entry(entry, Some(self.place)));
    }
}

impl Screen {
    /// Take the terminal. With `verbose`, the detail is printed in the
    /// conversation too. Fails, leaving the terminal as it was, when the
    /// terminal cannot be put in raw mode.
    pub(super) async fn start(verbose: bool) -> io::Result<Started> {
        let tty = File::options().read(true).write(true).open("/dev/tty")?;
        let (queue, inbox) = mpsc::unbounded_channel();
        let (typed_to, typed) = mpsc::unbounded_channel();
        let (interrupts, keys) = mpsc::unbounded_channel();
        let (failed, failures) = mpsc::unbounded_channel();
        let (ready, opened) = oneshot::channel();
        let links = Links {
            inbox,
            echo: queue.downgrade(),
            interrupts,
            failed,
        };
        std::thread::Builder::new()
            .name("screen".to_string())
            .spawn(move || serve(tty, links, typed_to, verbose, ready))?;
        opened
            .await
            .unwrap_or_else(|_| Err(io::Error::other("the screen's thread ended as it started")))?;
        let logs = {
            let queue = queue.clone();
            LogWriter::global().divert(move |record| {
                let _ = queue.send(ToScreen::Entry(Entry::from_record(record), None));
            })
        };
        Ok(Started {
            screen: Screen {
                queue,
                // The one being drawn, and `OUTBOX` more: the line mode's
                // bound, which `doc/reference/cli.md` states.
                outbox: Arc::new(Semaphore::new(OUTBOX + 1)),
                abandoned: AtomicBool::new(false),
                finished: false,
                logs: Some(logs),
                names: Mutex::default(),
            },
            typed,
            keys,
            failures,
        })
    }

    fn say(&self, item: ToScreen) {
        // A thread that has gone has said why through `Failures`.
        let _ = self.queue.send(item);
    }

    fn entry(&self, kind: Kind, text: &str) {
        self.say(ToScreen::Entry(Entry::new(kind, text), None));
    }

    /// A line in the conversation from the session itself.
    pub(super) fn note(&self, text: &str) {
        self.entry(Kind::Note, text);
    }

    /// An error, in the conversation.
    pub(super) fn error(&self, text: &str) {
        self.entry(Kind::Error, text);
    }

    /// A line of the agent's working, for the detail view.
    pub(super) fn detail(&self, text: &str) {
        self.entry(Kind::Detail, text);
    }

    /// One of the session's events.
    pub(super) fn event(&self, received: &Received) {
        let fact = {
            let mut names = lock(&self.names);
            names.observe(received);
            facts::fact(received, &names)
        };
        if let Some(fact) = fact {
            self.say(ToScreen::Fact(fact));
        }
    }

    /// A round ended: the model's own text and a reasoning-only report are
    /// detail, and the line it closes on is the conversation's.
    pub(super) fn ended(
        &self,
        reply: &str,
        reasoning: Option<String>,
        closing: Option<String>,
        still_running: Option<String>,
    ) {
        let mut entries = Vec::new();
        if !reply.trim().is_empty() {
            entries.push(Entry::titled(Kind::Detail, "the model said", reply));
        }
        if let Some(reasoning) = reasoning {
            entries.push(Entry::new(Kind::Detail, reasoning));
        }
        if let Some(closing) = closing {
            entries.push(Entry::new(Kind::Note, closing));
        }
        self.say(ToScreen::Ended {
            entries,
            still_running,
        });
    }

    /// The channel took a typed line, with `waiting` messages on it.
    pub(super) fn queued(&self, waiting: usize) {
        self.say(ToScreen::Queued(waiting));
    }

    /// A place for one more of the agent's messages, waiting until one of
    /// those before it is drawn. Cancel-safe.
    pub(super) async fn room(&self) -> Option<Room<'_>> {
        let place = Arc::clone(&self.outbox).acquire_owned().await.ok()?;
        Some(Room {
            place,
            screen: self,
        })
    }

    /// The user has asked to go: nothing waits on the screen from here.
    pub(super) fn abandon(&self) {
        self.abandoned.store(true, Ordering::SeqCst);
    }

    /// Wait for what was handed over to be drawn, unless the user has asked
    /// to go; when `interrupted` completes first, stop waiting, and do not
    /// wait again.
    pub(super) async fn settle(&self, interrupted: impl Future) {
        if self.abandoned.load(Ordering::SeqCst) {
            return;
        }
        let (done, written) = oneshot::channel();
        self.say(ToScreen::Written(done));
        tokio::select! {
            _ = written => {}
            _ = interrupted => self.abandon(),
        }
    }

    /// Show what is left and give the terminal back, waiting for the thread
    /// no longer than [`FINISH_GRACE`]; past that, the terminal is put back
    /// from here. Log records go to stderr again after.
    ///
    /// A signal that has put the terminal back is waited out as well: that
    /// the terminal is the shell's says nothing of whether the thread has
    /// drawn its last frame, and what the session writes next, as it closes,
    /// must not meet one. It stops at its next look, a tick away, and repairs
    /// what it drew in the meantime.
    pub(super) async fn finish(&mut self) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let (done, finished) = oneshot::channel();
        if self.queue.send(ToScreen::Finish(done)).is_ok() {
            tokio::select! {
                _ = finished => {}
                () = tokio::time::sleep(FINISH_GRACE) => tty::abandon(),
            }
        }
        self.logs = None;
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        // The thread sees the screen is dead, and stops drawing.
        if !self.finished {
            tty::abandon();
        }
    }
}

/// The thread's ends of its channels, but for the one typed lines go on.
struct Links {
    inbox: mpsc::UnboundedReceiver<ToScreen>,
    /// Where a sent line is shown: the queue the loop answers it on, which
    /// this does not keep open.
    echo: mpsc::WeakUnboundedSender<ToScreen>,
    interrupts: mpsc::UnboundedSender<()>,
    failed: mpsc::UnboundedSender<io::Error>,
}

/// The thread: open the screen, say whether it did, and run it. A failure, or
/// a panic, is reported before the sender typed lines go on is dropped, so
/// the loop reads a screen that died as a failure, and never as input ending.
fn serve(
    tty: File,
    links: Links,
    typed: mpsc::UnboundedSender<io::Result<String>>,
    verbose: bool,
    ready: oneshot::Sender<io::Result<()>>,
) {
    let mut terminal = match Terminal::open(tty, links, typed, verbose) {
        Ok(terminal) => terminal,
        Err(e) => {
            // A start given up on may have written after what put the
            // terminal back.
            tty::repair();
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    let ran = panic::catch_unwind(AssertUnwindSafe(|| terminal.run()));
    let ran = unless_taken_back(ran, tty::dead());
    if let Some(e) = failure(ran) {
        let _ = terminal.links.failed.send(e);
    }
    terminal.close();
}

/// `ran`, but for an error: a write that failed because the terminal was taken
/// back -- by a signal, after the frame's last look -- is the screen being
/// stopped, not failing, and reported would outrank the signal's exit status.
/// A panic is not this: it is the `Err` of `ran`, and is reported whatever took
/// the terminal.
fn unless_taken_back(
    ran: thread::Result<io::Result<()>>,
    taken_back: bool,
) -> thread::Result<io::Result<()>> {
    match ran {
        Ok(Err(_)) if taken_back => Ok(Ok(())),
        ran => ran,
    }
}

/// What a thread of the screen's ended on, if not on being done.
fn failure(ran: thread::Result<io::Result<()>>) -> Option<io::Error> {
    match ran {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(_) => Some(io::Error::other(
            "the screen stopped after an internal error",
        )),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What waits to be drawn: an entry, and its place in the outbox if it is one
/// of the agent's messages; or a marker. The place and the marker are held to
/// be dropped once what they stand for is drawn.
enum Pending {
    Entry {
        entry: Entry,
        _place: Option<OwnedSemaphorePermit>,
    },
    Written {
        _done: oneshot::Sender<()>,
    },
}

/// The input box, and where what it sends goes. Gone once input has ended,
/// and dropped otherwise only by the drawing thread, once it has reported why
/// it stopped.
struct Input {
    editor: Editor,
    typed: mpsc::UnboundedSender<io::Result<String>>,
}

/// The input box, shared: the keyboard's thread edits it, and the drawing
/// thread draws it.
type SharedInput = Arc<Mutex<Option<Input>>>;

/// What a key asks of the drawing thread, in the order the keys were read.
#[derive(Debug, PartialEq, Eq)]
enum Ui {
    /// The box changed, or input ended.
    Changed,
    OpenDetail,
    CloseDetail,
    Scroll(Scroll),
    Suspend,
    Redraw,
    Resized,
}

/// A move through the detail view.
#[derive(Debug, PartialEq, Eq)]
enum Scroll {
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// The keys, read on a thread of their own, which edits the box and sends
/// what it sends, delivers each Ctrl-C, and tells the drawing thread the rest.
struct Keyboard {
    input: SharedInput,
    echo: mpsc::WeakUnboundedSender<ToScreen>,
    interrupts: mpsc::UnboundedSender<()>,
    ui: std::sync::mpsc::Sender<Ui>,
    /// Whether the keys move through the detail view, rather than edit.
    detail: bool,
    stop: Arc<AtomicBool>,
}

impl Keyboard {
    /// Read keys until told to stop, or until the terminal was taken back.
    fn run(&mut self) -> io::Result<()> {
        // `tty::dead` ends this too: a signal that put the terminal back left
        // it un-raw, where a key read means nothing.
        while !self.stop.load(Ordering::SeqCst) && !tty::dead() {
            if !event::poll(TICK)? {
                continue;
            }
            match event::read()? {
                Event::Key(key) => self.key(key),
                Event::Paste(pasted) => {
                    let edit = match lock(&self.input).as_mut() {
                        Some(input) if !self.detail => input.editor.paste(&pasted),
                        _ => Edit::None,
                    };
                    if edit == Edit::Changed {
                        self.tell(Ui::Changed);
                    }
                }
                Event::Resize(..) => self.tell(Ui::Resized),
                _ => {}
            }
        }
        Ok(())
    }

    fn tell(&self, ui: Ui) {
        let _ = self.ui.send(ui);
    }

    fn key(&mut self, key: KeyEvent) {
        if self.detail {
            return self.detail_key(key);
        }
        let edit = match lock(&self.input).as_mut() {
            Some(input) => {
                let edit = input.editor.key(key);
                if let Edit::Submit(text) = &edit {
                    // Shown before the loop has it, on the queue it answers
                    // on, so that whatever it answers is shown after.
                    if let Some(echo) = self.echo.upgrade() {
                        let entry = Entry::new(Kind::User, text.as_str());
                        let _ = echo.send(ToScreen::Entry(entry, None));
                    }
                    let _ = input.typed.send(Ok(text.clone()));
                }
                edit
            }
            None => ended_key(key),
        };
        match edit {
            Edit::None | Edit::Submit(_) => {}
            Edit::Changed => self.tell(Ui::Changed),
            Edit::Eof => {
                *lock(&self.input) = None;
                self.tell(Ui::Changed);
            }
            Edit::Interrupt => {
                let _ = self.interrupts.send(());
            }
            Edit::Suspend => self.tell(Ui::Suspend),
            Edit::ToggleDetail => {
                self.detail = true;
                self.tell(Ui::OpenDetail);
            }
            Edit::Redraw => self.tell(Ui::Redraw),
        }
    }

    fn detail_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let scroll = match key.code {
            KeyCode::Char('c') if ctrl => {
                let _ = self.interrupts.send(());
                return;
            }
            // Suspending closes the view.
            KeyCode::Char('z') if ctrl => {
                self.detail = false;
                return self.tell(Ui::Suspend);
            }
            KeyCode::Char('o') if ctrl => {
                self.detail = false;
                return self.tell(Ui::CloseDetail);
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.detail = false;
                return self.tell(Ui::CloseDetail);
            }
            KeyCode::Up | KeyCode::Char('k') => Scroll::Up,
            KeyCode::Down | KeyCode::Char('j') => Scroll::Down,
            KeyCode::PageUp | KeyCode::Char('b') => Scroll::PageUp,
            KeyCode::PageDown | KeyCode::Char(' ') => Scroll::PageDown,
            KeyCode::Home | KeyCode::Char('g') => Scroll::Top,
            KeyCode::End | KeyCode::Char('G') => Scroll::Bottom,
            _ => return,
        };
        self.tell(Ui::Scroll(scroll));
    }
}

/// The keyboard's thread, as the drawing thread holds it.
struct Keys {
    ui: std::sync::mpsc::Receiver<Ui>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Keys {
    /// Start reading keys into `input`, showing each line sent on `echo`. A
    /// failure is reported on `failed`; the sender typed lines go on is the
    /// drawing thread's to drop.
    fn start(
        input: SharedInput,
        echo: mpsc::WeakUnboundedSender<ToScreen>,
        interrupts: mpsc::UnboundedSender<()>,
        failed: mpsc::UnboundedSender<io::Error>,
    ) -> io::Result<Self> {
        let (ui, heard) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut keyboard = Keyboard {
            input,
            echo,
            interrupts,
            ui,
            detail: false,
            stop: Arc::clone(&stop),
        };
        let thread = thread::Builder::new()
            .name("keys".to_string())
            .spawn(move || {
                let ran = panic::catch_unwind(AssertUnwindSafe(|| keyboard.run()));
                if let Some(e) = failure(ran) {
                    let _ = failed.send(e);
                }
            })?;
        Ok(Self {
            ui: heard,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop reading keys, and wait until nothing reads them.
    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The detail view: its panel over the whole alternate screen, how far it is
/// scrolled up from the bottom, and whether the terminal changed size while
/// it was open, which moves the conversation's viewport.
struct Detail {
    panel: Panel,
    scroll: usize,
    resized: bool,
}

struct Terminal {
    out: CrosstermBackend<BufWriter<Held>>,
    raw: Raw,
    viewport: Viewport,
    detail: Option<Detail>,
    input: SharedInput,
    keys: Keys,
    status: Status,
    transcript: Transcript,
    pending: VecDeque<Pending>,
    links: Links,
    verbose: bool,
    theme: Theme,
    /// Something changed since the last frame.
    dirty: bool,
    /// The terminal changed size since the last frame.
    resized: bool,
    spun: Instant,
    frame: usize,
    finish: Option<oneshot::Sender<()>>,
}

impl Terminal {
    fn open(
        tty: File,
        links: Links,
        typed: mpsc::UnboundedSender<io::Result<String>>,
        verbose: bool,
    ) -> io::Result<Self> {
        let mut out = CrosstermBackend::new(BufWriter::new(Held::new(tty)));
        let raw = Raw::enter(&mut out)?;
        let editor = Editor::default();
        let layout = render::layout(&editor, out.size()?.width);
        let viewport = Viewport::open(&mut out, render::viewport_height(Some(&layout)))?;
        let input = Arc::new(Mutex::new(Some(Input { editor, typed })));
        let keys = Keys::start(
            Arc::clone(&input),
            links.echo.clone(),
            links.interrupts.clone(),
            links.failed.clone(),
        )?;
        Ok(Self {
            out,
            raw,
            viewport,
            detail: None,
            input,
            keys,
            status: Status::default(),
            transcript: Transcript::default(),
            pending: VecDeque::new(),
            links,
            verbose,
            theme: Theme::from_env(),
            dirty: true,
            resized: false,
            spun: Instant::now(),
            frame: 0,
            finish: None,
        })
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            if tty::dead() {
                // A signal put the terminal back and the session is ending:
                // the screen is done, not failed. Reported as a failure it
                // would become the session's error and outrank the signal's
                // own exit status.
                if tty::signalled() {
                    return Ok(());
                }
                return Err(io::Error::other("the screen stopped after a panic"));
            }
            self.heed()?;
            let finishing = self.take();
            self.show()?;
            if finishing {
                return Ok(());
            }
        }
    }

    /// Do what the keys asked, waiting up to [`TICK`] for the first of it.
    fn heed(&mut self) -> io::Result<()> {
        let mut next = match self.keys.ui.recv_timeout(TICK) {
            Ok(ui) => Some(ui),
            Err(RecvTimeoutError::Timeout) => None,
            // The keyboard's thread has stopped, and said why if it failed.
            Err(RecvTimeoutError::Disconnected) => {
                thread::sleep(TICK);
                None
            }
        };
        while let Some(ui) = next {
            self.apply(ui)?;
            next = self.keys.ui.try_recv().ok();
        }
        Ok(())
    }

    fn apply(&mut self, ui: Ui) -> io::Result<()> {
        match ui {
            Ui::Changed => self.dirty = true,
            Ui::OpenDetail if self.detail.is_none() => self.open_detail()?,
            Ui::CloseDetail if self.detail.is_some() => self.close_detail()?,
            Ui::OpenDetail | Ui::CloseDetail => {}
            Ui::Scroll(scroll) => self.scroll(scroll),
            Ui::Suspend => self.suspend()?,
            Ui::Redraw => {
                self.viewport.redraw(&mut self.out)?;
                self.dirty = true;
            }
            Ui::Resized => self.resized = true,
        }
        Ok(())
    }

    fn scroll(&mut self, scroll: Scroll) {
        let page = usize::from(self.viewport.screen().height.saturating_sub(2)).max(1);
        let Some(detail) = &mut self.detail else {
            return;
        };
        detail.scroll = match scroll {
            Scroll::Up => detail.scroll.saturating_add(1),
            Scroll::Down => detail.scroll.saturating_sub(1),
            Scroll::PageUp => detail.scroll.saturating_add(page),
            Scroll::PageDown => detail.scroll.saturating_sub(page),
            Scroll::Top => usize::MAX,
            Scroll::Bottom => 0,
        };
        self.dirty = true;
    }

    /// Take what the loop handed over. Returns whether it is time to finish.
    fn take(&mut self) -> bool {
        loop {
            let item = match self.links.inbox.try_recv() {
                Ok(item) => item,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            };
            match item {
                ToScreen::Entry(entry, place) => self.record(entry, place),
                ToScreen::Fact(fact) => {
                    self.dirty |= self.status.apply(&fact, Instant::now());
                    if let Some(entry) = Entry::from_fact(&fact) {
                        self.record(entry, None);
                    }
                }
                ToScreen::Ended {
                    entries,
                    still_running,
                } => {
                    for entry in entries {
                        self.record(entry, None);
                    }
                    if !self.status.ended(still_running) {
                        self.record(
                            Entry::new(
                                Kind::Hint,
                                "(the agent sent you nothing this round; Ctrl-O shows what it did)",
                            ),
                            None,
                        );
                    }
                    self.dirty = true;
                }
                ToScreen::Queued(waiting) => {
                    self.status.queued(waiting);
                    self.dirty = true;
                }
                ToScreen::Written(done) => self.pending.push_back(Pending::Written { _done: done }),
                ToScreen::Finish(done) => {
                    self.finish = Some(done);
                    return true;
                }
            }
        }
    }

    /// Keep `entry`, and queue it to be drawn in the conversation if it shows
    /// there, with `place` held until it is. One that shows only in the
    /// detail view changes a frame only while that is open.
    fn record(&mut self, entry: Entry, place: Option<OwnedSemaphorePermit>) {
        if entry.inline(self.verbose) {
            self.pending.push_back(Pending::Entry {
                entry: entry.clone(),
                _place: place,
            });
        }
        self.transcript.push(entry, self.theme);
        self.dirty |= self.detail.is_some();
    }

    /// Draw a frame, if anything changed.
    fn show(&mut self) -> io::Result<()> {
        // A signal may have put the terminal back since the loop last looked,
        // while this thread waited for a key: nothing is drawn over it.
        if tty::dead() {
            return Ok(());
        }
        if std::mem::take(&mut self.resized) {
            match &mut self.detail {
                Some(detail) => {
                    detail.resized = true;
                    self.reopen_detail()?;
                }
                None => self.viewport.resized(&mut self.out)?,
            }
            self.dirty = true;
        }
        if self.detail.is_some() {
            if std::mem::take(&mut self.dirty) {
                self.draw_detail()?;
            }
            return Ok(());
        }
        if self.status.busy() && self.spun.elapsed() >= SPIN {
            self.frame += 1;
            self.spun = Instant::now();
            self.dirty = true;
        }
        if !std::mem::take(&mut self.dirty) && self.pending.is_empty() {
            return Ok(());
        }
        queue!(self.out, BeginSynchronizedUpdate)?;
        let drawn = self.flush_pending()?;
        let width = self.viewport.screen().width;
        let input = lock(&self.input)
            .as_ref()
            .map(|input| render::layout(&input.editor, width));
        self.viewport
            .set_height(&mut self.out, render::viewport_height(input.as_ref()))?;
        let shown = self.status.shown(Instant::now());
        let hint = match (&input, shown.busy) {
            (Some(_), true) => "Ctrl-C stops · Ctrl-O detail",
            (Some(_), false) => "Alt-Enter newline · Ctrl-O detail",
            (None, _) => "Ctrl-O detail",
        };
        let (frame, theme) = (self.frame, self.theme);
        self.viewport.draw(&mut self.out, |buf| {
            render::draw_viewport(buf, &shown, frame, hint, input.as_ref(), theme)
        })?;
        tty::viewport_at(self.viewport.area().y);
        queue!(self.out, EndSynchronizedUpdate)?;
        self.out.flush()?;
        drop(drawn);
        Ok(())
    }

    /// Put what waits into the conversation. Returns what went in, to drop
    /// once it is flushed to the terminal: that gives the agent's places in
    /// the outbox back, and tells whatever waits on a marker it was drawn.
    fn flush_pending(&mut self) -> io::Result<Vec<Pending>> {
        let width = self.viewport.screen().width;
        let drawn: Vec<Pending> = self.pending.drain(..).collect();
        let mut lines = Vec::new();
        for pending in &drawn {
            if let Pending::Entry { entry, .. } = pending {
                let dim = entry.kind == Kind::Detail;
                lines.extend(render::entry_lines(entry, width, self.theme, dim));
            }
        }
        if !lines.is_empty() {
            self.viewport.insert(&mut self.out, &lines)?;
        }
        Ok(drawn)
    }

    fn open_detail(&mut self) -> io::Result<()> {
        queue!(self.out, EnterAlternateScreen)?;
        tty::detail_open(true);
        self.detail = Some(Detail {
            panel: Panel::cleared(Rect::default()),
            scroll: 0,
            resized: false,
        });
        self.reopen_detail()?;
        self.dirty = true;
        Ok(())
    }

    /// The detail view's panel over the whole screen, cleared.
    fn reopen_detail(&mut self) -> io::Result<()> {
        let size = self.out.size()?;
        queue!(self.out, Clear(ClearType::All))?;
        if let Some(detail) = &mut self.detail {
            detail.panel = Panel::cleared(Rect::new(0, 0, size.width, size.height));
        }
        Ok(())
    }

    fn draw_detail(&mut self) -> io::Result<()> {
        let Some(Detail { panel, scroll, .. }) = &mut self.detail else {
            return Ok(());
        };
        let width = panel.area().width;
        let dropped = self.transcript.dropped();
        let lines = self.transcript.lines(width, self.theme);
        panel.draw(&mut self.out, |buf| {
            *scroll = render::draw_detail(buf, lines, *scroll, dropped);
            None
        })?;
        self.out.flush()
    }

    fn close_detail(&mut self) -> io::Result<()> {
        // Written before the view is marked closed: until it has been, a
        // signal's restore leaves the alternate screen itself.
        queue!(self.out, LeaveAlternateScreen)?;
        self.out.flush()?;
        tty::detail_open(false);
        self.transcript.forget_lines();
        // The main screen comes back as it was left, unless the terminal
        // changed size meanwhile.
        if self.detail.take().is_some_and(|detail| detail.resized) {
            self.viewport.resized(&mut self.out)?;
        }
        self.dirty = true;
        Ok(())
    }

    /// Ctrl-Z: clear the viewport, so the shell's word about the stop lands
    /// there, stop, and take the terminal again where the cursor is.
    fn suspend(&mut self) -> io::Result<()> {
        if self.detail.is_some() {
            self.close_detail()?;
        }
        self.viewport.clear(&mut self.out)?;
        self.out.flush()?;
        self.raw.suspend(&mut self.out)?;
        let height = self.viewport.area().height;
        self.viewport = Viewport::open(&mut self.out, height)?;
        self.dirty = true;
        Ok(())
    }

    /// Show what is left and put the terminal back, best effort: whatever
    /// fails here, the terminal's modes are put back as `raw` is dropped.
    fn close(&mut self) {
        // Nothing reads keys once the terminal is no longer raw.
        self.keys.stop();
        if !tty::dead() {
            let _ = (|| -> io::Result<()> {
                if self.detail.is_some() {
                    self.close_detail()?;
                }
                let drawn = self.flush_pending()?;
                self.viewport.clear(&mut self.out)?;
                self.out.flush()?;
                self.raw.leave(&mut self.out)?;
                drop(drawn);
                Ok(())
            })();
        }
        // Queued escapes the dead-state checks kept from being flushed would
        // otherwise be written as `out` drops, after the repair.
        let _ = self.out.flush();
        // After the last write there will be: whatever a signal's restore
        // met this thread in the middle of is put right.
        tty::repair();
        if let Some(done) = self.finish.take() {
            let _ = done.send(());
        }
    }
}

/// A key once input has ended: Ctrl-C, Ctrl-O, Ctrl-L and Ctrl-Z still do
/// what they did, and nothing else does anything.
fn ended_key(key: KeyEvent) -> Edit {
    if key.kind == KeyEventKind::Release || !key.modifiers.contains(KeyModifiers::CONTROL) {
        return Edit::None;
    }
    match key.code {
        KeyCode::Char('c') => Edit::Interrupt,
        KeyCode::Char('o') => Edit::ToggleDetail,
        KeyCode::Char('l') => Edit::Redraw,
        KeyCode::Char('z') => Edit::Suspend,
        _ => Edit::None,
    }
}

/// Each row of `buf` as text, trailing spaces trimmed.
#[cfg(test)]
fn rows_of(buf: &ratatui::buffer::Buffer) -> Vec<String> {
    let area = buf.area;
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write refused because a signal took the terminal back is the screen
    /// stopping, so the session keeps the signal's exit status (#644); a write
    /// that failed on its own, or a panic, is still a failure.
    #[test]
    fn a_write_refused_after_the_terminal_was_taken_back_is_not_a_failure() {
        let refused = || Ok(Err(io::Error::other("refused")));
        assert!(failure(unless_taken_back(refused(), true)).is_none());
        assert!(failure(unless_taken_back(refused(), false)).is_some());
        let panicked = Err(Box::new("boom") as Box<dyn std::any::Any + Send>);
        assert!(failure(unless_taken_back(panicked, true)).is_some());
    }

    #[test]
    fn the_screen_wants_stdin_and_stdout_on_the_controlling_terminal() {
        let term = Some("xterm-256color");
        assert!(decide(true, true, term, true));
        assert!(decide(true, true, None, true));
        assert!(!decide(true, true, term, false), "stdout redirected");
        assert!(
            !decide(true, false, term, true),
            "another terminal on stdin"
        );
        assert!(!decide(false, false, term, true), "piped stdin");
        assert!(!decide(true, true, Some("dumb"), true));
    }

    /// A keyboard, its box empty, showing what it sends on `echo`; and where
    /// it sends lines, Ctrl-C, and what it tells the drawing thread.
    fn keyboard(
        echo: &mpsc::UnboundedSender<ToScreen>,
    ) -> (
        Keyboard,
        Typed,
        mpsc::UnboundedReceiver<()>,
        std::sync::mpsc::Receiver<Ui>,
    ) {
        let (typed_to, typed) = mpsc::unbounded_channel();
        let (interrupts, pressed) = mpsc::unbounded_channel();
        let (ui, heard) = std::sync::mpsc::channel();
        let keyboard = Keyboard {
            input: Arc::new(Mutex::new(Some(Input {
                editor: Editor::default(),
                typed: typed_to,
            }))),
            echo: echo.downgrade(),
            interrupts,
            ui,
            detail: false,
            stop: Arc::default(),
        };
        (keyboard, typed, pressed, heard)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn send(keyboard: &mut Keyboard, line: &str) {
        for c in line.chars() {
            keyboard.key(press(KeyCode::Char(c)));
        }
        keyboard.key(press(KeyCode::Enter));
    }

    /// The keyboard's thread sends what is typed and each Ctrl-C itself, and
    /// only tells the drawing thread what to show: a drawing thread held up
    /// in a write holds neither up.
    #[test]
    fn the_keyboard_delivers_lines_and_interrupts_itself() {
        let (queue, _inbox) = mpsc::unbounded_channel();
        let (mut keyboard, mut typed, mut pressed, heard) = keyboard(&queue);
        send(&mut keyboard, "/q");
        assert_eq!(typed.try_recv().expect("sent").expect("a line"), "/q");

        // In the detail view, keys move through it, and Ctrl-C still goes.
        keyboard.key(ctrl('o'));
        keyboard.key(press(KeyCode::Char('k')));
        keyboard.key(ctrl('c'));
        keyboard.key(press(KeyCode::Esc));
        assert!(pressed.try_recv().is_ok(), "Ctrl-C in the detail view");
        let told: Vec<Ui> = heard.try_iter().collect();
        assert_eq!(
            told,
            [
                Ui::Changed,
                Ui::Changed,
                Ui::OpenDetail,
                Ui::Scroll(Scroll::Up),
                Ui::CloseDetail,
            ]
        );

        keyboard.key(ctrl('d'));
        assert!(
            matches!(typed.try_recv(), Err(TryRecvError::Disconnected)),
            "Ctrl-D at an empty box ends input"
        );
    }

    /// The loop, answering on the screen's queue the moment a line is handed
    /// to it, as it is handed: the soonest an answer could come.
    struct Answers(mpsc::UnboundedSender<ToScreen>);

    impl std::task::Wake for Answers {
        fn wake(self: Arc<Self>) {
            let _ = self.0.send(ToScreen::Entry(
                Entry::new(Kind::Note, "the commands"),
                None,
            ));
        }
    }

    /// A sent line is queued to be shown before the loop has it, on the queue
    /// the loop answers on, so an answer -- `/help`'s, say -- is never shown
    /// above the line that asked for it, however the drawing thread's turns
    /// fall.
    #[test]
    fn a_sent_line_is_shown_before_the_loop_can_answer_it() {
        let (queue, mut inbox) = mpsc::unbounded_channel();
        let (mut keyboard, mut typed, _pressed, _heard) = keyboard(&queue);
        let answers = std::task::Waker::from(Arc::new(Answers(queue.clone())));
        let mut waiting = std::task::Context::from_waker(&answers);
        assert!(typed.poll_recv(&mut waiting).is_pending());
        send(&mut keyboard, "/help");
        assert_eq!(typed.try_recv().expect("sent").expect("a line"), "/help");

        let mut shown = Vec::new();
        while let Ok(ToScreen::Entry(entry, _)) = inbox.try_recv() {
            shown.push((entry.kind, entry.text));
        }
        assert_eq!(
            shown,
            [
                (Kind::User, "/help".to_string()),
                (Kind::Note, "the commands".to_string())
            ]
        );
    }

    #[test]
    fn after_input_ends_only_the_screens_own_keys_work() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        assert_eq!(ended_key(ctrl('c')), Edit::Interrupt);
        assert_eq!(ended_key(ctrl('o')), Edit::ToggleDetail);
        assert_eq!(ended_key(ctrl('d')), Edit::None);
        assert_eq!(
            ended_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Edit::None
        );
    }
}
