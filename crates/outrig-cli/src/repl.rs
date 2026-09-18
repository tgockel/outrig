//! Interactive stdin/stdout REPL with slash commands.
//!
//! `Repl::run` drives the I/O loop: print a banner on stderr, prompt with `> `,
//! and feed each non-slash line to a caller-supplied async callback. Only
//! `/help` and `/quit` are built in; every other slash command belongs to the
//! caller, which supplies its help line (via [`HelpEntry`]) and its semantics
//! (via the `on_command` dispatcher). The dispatcher receives the command
//! name and its whitespace-split arguments and returns the stderr text, or
//! `None` for a command it doesn't know -- the REPL then prints the
//! unknown-command notice. EOF (Ctrl-D) and `/quit` exit cleanly. SIGINT
//! during a callback cancels the in-flight future, prints
//! `[outrig] interrupted`, and returns to the prompt; a second consecutive
//! SIGINT (no input typed in between) exits.
//!
//! Strict stream separation: assistant text goes to stdout; everything else --
//! banner, prompt, slash-command output, interrupt notice -- goes to stderr.
//! That way `outrig run > out.txt` captures only the model's replies.
//!
//! Lines arrive through a [`LineSource`], of which there are two. On an
//! interactive terminal `Repl::run` picks
//! [`EditorSource`](editor::EditorSource), a rustyline editor that gives the
//! prompt readline-style editing and recall of this session's earlier prompts,
//! and that draws on `/dev/tty` rather than on stderr. Piped stdin -- and every
//! integration test -- gets [`StreamSource`], which reads a line at a time and
//! leaves the terminal in canonical mode. Everything below the read is shared,
//! so the two differ only in how a line and a `Ctrl-C` reach the loop.
//!
//! [`Repl::run_with`] is the stream form spelled out: any `AsyncBufRead` /
//! `AsyncWrite` pair plus an interrupt-future factory, which is what lets the
//! integration tests drive the loop over `tokio::io::duplex` halves and a
//! `tokio::sync::Notify`.

mod editor;
mod line_source;
// Two arms rather than the crate's `internal_modules!` macro, which only
// reaches crate-root modules: the pty harness in `examples/` needs to raise a
// notice while a prompt is up, which is the one thing about this module that
// cannot be checked without a terminal.
#[cfg(feature = "internal-test-api")]
pub mod notice;
#[cfg(not(feature = "internal-test-api"))]
pub(crate) mod notice;

use std::fmt::Write as _;
use std::future::Future;

use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};

use self::line_source::{LineEvent, LineSource, StreamSource};
use crate::error::Result;

/// One caller-owned slash command's `/help` line. `syntax` carries the
/// leading `/` and any argument placeholders (e.g. `/sidecar add <name>`);
/// the REPL owns layout, so entries stay aligned with the built-in `/help`
/// and `/quit` lines no matter how long the syntax column grows.
pub struct HelpEntry {
    pub syntax: &'static str,
    pub description: &'static str,
}

const HELP_BUILTIN_FIRST: HelpEntry = HelpEntry {
    syntax: "/help",
    description: "show this help",
};

const HELP_BUILTIN_LAST: HelpEntry = HelpEntry {
    syntax: "/quit",
    description: "exit the session",
};

/// Compose the `/help` text: header, built-in `/help`, the caller's entries
/// in order, built-in `/quit` -- one syntax column padded across all lines.
pub(crate) fn compose_help(commands: &[HelpEntry]) -> String {
    let all = std::iter::once(&HELP_BUILTIN_FIRST)
        .chain(commands)
        .chain(std::iter::once(&HELP_BUILTIN_LAST));
    let pad = all
        .clone()
        .map(|entry| entry.syntax.len())
        .max()
        .expect("built-ins make the iterator non-empty");
    let mut buf = String::from("[outrig] slash commands:\n");
    for entry in all {
        let _ = writeln!(buf, "  {:<pad$}   {}", entry.syntax, entry.description);
    }
    buf
}

const INTERRUPT_NOTICE: &[u8] = b"\n[outrig] interrupted\n";

const PROMPT: &str = "> ";

pub struct Repl;

impl Repl {
    /// Run the REPL against the real terminal, treating
    /// `tokio::signal::ctrl_c()` as the interrupt source. The `banner` is
    /// printed once to stderr before the first prompt; `on_prompt` is invoked
    /// for every non-slash, non-empty input line and its non-empty returned
    /// text is printed to stdout. Streaming callers may write incrementally
    /// during the callback and return an empty string to suppress trailing
    /// reprint.
    ///
    /// Slash commands other than the built-in `/help` and `/quit` go to
    /// `on_command` as `(name, whitespace-split args)` -- `("sidecar",
    /// ["add", "tools"])`, `("tools", [])`. `Some(text)` is printed to
    /// stderr (side effects are the caller's); `None` means the command is
    /// unknown and the REPL prints the notice. `commands` supplies the
    /// caller commands' `/help` lines.
    pub async fn run<P, PFut, C, CFut>(
        banner: &str,
        commands: &[HelpEntry],
        on_prompt: P,
        on_command: C,
    ) -> Result<()>
    where
        P: FnMut(String) -> PFut,
        PFut: Future<Output = Result<String>>,
        C: FnMut(String, Vec<String>) -> CFut,
        CFut: Future<Output = Option<String>>,
    {
        Self::run_loop(
            line_source::auto(),
            tokio::io::stdout(),
            tokio::io::stderr(),
            ctrl_c_signal,
            banner,
            commands,
            on_prompt,
            on_command,
        )
        .await
    }

    /// Generic form parameterized over the I/O streams and interrupt source:
    /// the testable seam `plan/done/0018` asks for. Integration tests call it
    /// with `tokio::io::duplex` halves and a `Notify`-driven interrupt closure
    /// to exercise EOF, slash commands, and SIGINT handling without touching
    /// real signals or terminals.
    ///
    /// Production no longer reaches it -- [`Repl::run`] asks
    /// [`line_source::auto`] for a source and goes straight to `run_loop` --
    /// but what it wraps `stdin` in, [`StreamSource`], is exactly what `auto`
    /// falls back to off a terminal, and the loop below it is the same code.
    #[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with<RD, W, E, I, IFut, P, PFut, C, CFut>(
        stdin: RD,
        stdout: W,
        stderr: E,
        interrupt: I,
        banner: &str,
        commands: &[HelpEntry],
        on_prompt: P,
        on_command: C,
    ) -> Result<()>
    where
        RD: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
        E: AsyncWrite + Unpin,
        I: FnMut() -> IFut,
        IFut: Future<Output = ()>,
        P: FnMut(String) -> PFut,
        PFut: Future<Output = Result<String>>,
        C: FnMut(String, Vec<String>) -> CFut,
        CFut: Future<Output = Option<String>>,
    {
        Self::run_loop(
            StreamSource::new(stdin),
            stdout,
            stderr,
            interrupt,
            banner,
            commands,
            on_prompt,
            on_command,
        )
        .await
    }

    /// The loop itself, over whichever [`LineSource`] the caller picked.
    #[allow(clippy::too_many_arguments)]
    async fn run_loop<S, W, E, I, IFut, P, PFut, C, CFut>(
        mut source: S,
        mut stdout: W,
        mut stderr: E,
        mut interrupt: I,
        banner: &str,
        commands: &[HelpEntry],
        mut on_prompt: P,
        mut on_command: C,
    ) -> Result<()>
    where
        S: LineSource,
        W: AsyncWrite + Unpin,
        E: AsyncWrite + Unpin,
        I: FnMut() -> IFut,
        IFut: Future<Output = ()>,
        P: FnMut(String) -> PFut,
        PFut: Future<Output = Result<String>>,
        C: FnMut(String, Vec<String>) -> CFut,
        CFut: Future<Output = Option<String>>,
    {
        if !banner.is_empty() {
            stderr.write_all(banner.as_bytes()).await?;
            if !banner.ends_with('\n') {
                stderr.write_all(b"\n").await?;
            }
            stderr.flush().await?;
        }

        let help_text = compose_help(commands);
        let mut last_was_interrupt = false;

        loop {
            let event = source.read_line(PROMPT, &mut stderr, interrupt()).await?;

            let line = match event {
                LineEvent::Eof => return Ok(()),
                LineEvent::Interrupted => {
                    if last_was_interrupt {
                        return Ok(());
                    }
                    last_was_interrupt = true;
                    continue;
                }
                LineEvent::Line(line) => line,
            };

            let trimmed = line.trim_end_matches(['\r', '\n']);

            if trimmed.is_empty() {
                continue;
            }

            last_was_interrupt = false;

            if let Some(raw) = trimmed.strip_prefix('/') {
                let mut tokens = raw.split_whitespace();
                let name = tokens.next().unwrap_or("");
                let args: Vec<String> = tokens.map(str::to_string).collect();
                match (name, args.is_empty()) {
                    ("quit", true) => return Ok(()),
                    ("help", true) => {
                        write_stderr_line(&mut stderr, &help_text).await?;
                    }
                    _ => match on_command(name.to_string(), args).await {
                        Some(text) => write_stderr_line(&mut stderr, &text).await?,
                        None => {
                            // `raw`, not name + args: the notice echoes
                            // the input as typed.
                            let notice = format!("[outrig] unknown command: /{raw}");
                            write_stderr_line(&mut stderr, &notice).await?;
                        }
                    },
                }
                continue;
            }

            tokio::select! {
                res = on_prompt(trimmed.to_string()) => {
                    let reply = res?;
                    if !reply.is_empty() {
                        stdout.write_all(reply.as_bytes()).await?;
                        if !reply.ends_with('\n') {
                            stdout.write_all(b"\n").await?;
                        }
                        stdout.flush().await?;
                    }
                }
                _ = interrupt() => {
                    stderr.write_all(INTERRUPT_NOTICE).await?;
                    stderr.flush().await?;
                    last_was_interrupt = true;
                }
            }
        }
    }
}

async fn ctrl_c_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

async fn write_stderr_line<E>(stderr: &mut E, text: &str) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    stderr.write_all(text.as_bytes()).await?;
    if !text.ends_with('\n') {
        stderr.write_all(b"\n").await?;
    }
    stderr.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future;
    use std::rc::Rc;

    use super::*;

    /// A [`LineSource`] that replays a fixed script and, like the rustyline
    /// editor, uses neither the stderr sink nor the interrupt future it is
    /// lent. That makes these drive the loop down exactly the editor's path --
    /// `Ctrl-C` arriving as a `LineEvent` rather than as a signal -- with no
    /// pty, terminal, or real signal anywhere.
    struct ScriptedSource {
        events: Rc<RefCell<VecDeque<LineEvent>>>,
    }

    impl LineSource for ScriptedSource {
        async fn read_line<E, F>(
            &mut self,
            _prompt: &str,
            _out: &mut E,
            _interrupt: F,
        ) -> Result<LineEvent>
        where
            E: AsyncWrite + Unpin,
            F: Future<Output = ()>,
        {
            Ok(self
                .events
                .borrow_mut()
                .pop_front()
                .unwrap_or(LineEvent::Eof))
        }
    }

    /// The script, plus a handle on what is left of it so a test can assert the
    /// loop stopped reading rather than merely stopped acting.
    fn script(events: Vec<LineEvent>) -> (ScriptedSource, Rc<RefCell<VecDeque<LineEvent>>>) {
        let events: Rc<RefCell<VecDeque<LineEvent>>> = Rc::new(RefCell::new(events.into()));
        (
            ScriptedSource {
                events: events.clone(),
            },
            events,
        )
    }

    fn line(text: &str) -> LineEvent {
        LineEvent::Line(text.to_string())
    }

    /// Runs the script with no caller commands, returning the prompts
    /// `on_prompt` saw and whatever reached stderr.
    async fn drive(source: ScriptedSource) -> (Vec<String>, String) {
        let seen: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let seen_cb = seen.clone();
        let on_prompt = move |text: String| {
            let seen = seen_cb.clone();
            async move {
                seen.borrow_mut().push(text);
                Result::Ok(String::new())
            }
        };

        let mut stderr = Vec::new();
        Repl::run_loop(
            source,
            Vec::new(),
            &mut stderr,
            future::pending::<()>,
            "",
            &[],
            on_prompt,
            |_, _| future::ready(None),
        )
        .await
        .expect("the loop must end cleanly");

        let prompts = seen.borrow().clone();
        (prompts, String::from_utf8(stderr).expect("stderr utf-8"))
    }

    /// rustyline writes a newline on every `readline` return path, so the loop
    /// must not add one. A blank line here would show up under every Ctrl-C.
    #[tokio::test]
    async fn a_terminal_source_owns_its_own_line_endings() {
        let (source, _) = script(vec![LineEvent::Interrupted, LineEvent::Eof]);
        let (prompts, stderr) = drive(source).await;
        assert!(prompts.is_empty());
        assert!(
            stderr.is_empty(),
            "the loop must paint nothing for a terminal-owning source: {stderr:?}",
        );
    }

    #[tokio::test]
    async fn two_interrupts_in_a_row_exit_without_reading_further() {
        let (source, rest) = script(vec![
            LineEvent::Interrupted,
            LineEvent::Interrupted,
            line("must never be read"),
        ]);
        let (prompts, _) = drive(source).await;
        assert!(prompts.is_empty());
        assert_eq!(
            rest.borrow().len(),
            1,
            "the second interrupt must exit before the next line is read",
        );
    }

    #[tokio::test]
    async fn a_prompt_between_two_interrupts_resets_the_exit() {
        let (source, _) = script(vec![
            LineEvent::Interrupted,
            line("hello"),
            LineEvent::Interrupted,
            LineEvent::Interrupted,
        ]);
        let (prompts, _) = drive(source).await;
        assert_eq!(prompts, vec!["hello".to_string()]);
    }

    /// Pins a pre-existing quirk rather than endorsing it: the empty-line
    /// `continue` sits above the flag reset, so a bare Enter does *not* count
    /// as the intervening input that disarms a second Ctrl-C.
    #[tokio::test]
    async fn an_empty_line_does_not_disarm_the_second_interrupt() {
        let (source, rest) = script(vec![
            LineEvent::Interrupted,
            line(""),
            LineEvent::Interrupted,
            line("must never be read"),
        ]);
        let (prompts, _) = drive(source).await;
        assert!(prompts.is_empty());
        assert_eq!(rest.borrow().len(), 1, "the second interrupt must exit");
    }

    /// Dispatch must not vary with the source: the same slash handling the
    /// stream-backed tests in `tests/repl_io.rs` cover has to hold here.
    #[tokio::test]
    async fn slash_commands_dispatch_the_same_through_a_terminal_source() {
        let (source, rest) = script(vec![
            line("/help"),
            line("/bogus arg"),
            line("/quit"),
            line("x"),
        ]);
        let (prompts, stderr) = drive(source).await;
        assert!(prompts.is_empty(), "no slash command is a model prompt");
        assert!(stderr.contains("[outrig] slash commands:"), "{stderr:?}");
        assert!(
            stderr.contains("[outrig] unknown command: /bogus arg\n"),
            "{stderr:?}",
        );
        assert_eq!(rest.borrow().len(), 1, "/quit must stop the loop");
    }

    /// The mixed case the two interrupt mechanisms have to agree on: SIGINT
    /// cancels a turn (the terminal is in canonical mode then, so it really is
    /// a signal), and the very next Ctrl-C at the prompt -- a keystroke the
    /// editor decodes -- exits.
    #[tokio::test]
    async fn an_interrupted_turn_arms_the_next_interrupt_at_the_prompt() {
        let (source, rest) = script(vec![
            line("slow"),
            LineEvent::Interrupted,
            line("must never be read"),
        ]);

        let mut stderr = Vec::new();
        Repl::run_loop(
            source,
            Vec::new(),
            &mut stderr,
            // Fires the moment the turn starts, so `on_prompt` never finishes.
            || future::ready(()),
            "",
            &[],
            |_: String| future::pending::<Result<String>>(),
            |_, _| future::ready(None),
        )
        .await
        .expect("the loop must end cleanly");

        assert_eq!(
            rest.borrow().len(),
            1,
            "the interrupt after a cancelled turn must exit",
        );
        let stderr = String::from_utf8(stderr).expect("stderr utf-8");
        assert!(stderr.contains("[outrig] interrupted"), "{stderr:?}");
    }
}
