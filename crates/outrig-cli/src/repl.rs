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
//! [`Repl::run_with`] is the generic form: it takes any `AsyncBufRead` /
//! `AsyncWrite` streams plus an interrupt-future factory, so integration tests
//! can substitute `tokio::io::duplex` halves and a `tokio::sync::Notify`-driven
//! interrupt source.

use std::fmt::Write as _;
use std::future::Future;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

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

pub struct Repl;

impl Repl {
    /// Run the REPL against real stdin/stdout/stderr, treating
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
        let stdin = BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        let stderr = tokio::io::stderr();
        Self::run_with(
            stdin,
            stdout,
            stderr,
            ctrl_c_signal,
            banner,
            commands,
            on_prompt,
            on_command,
        )
        .await
    }

    /// Generic form parameterized over the I/O streams and interrupt source.
    /// Production calls this with real handles via [`Repl::run`]; integration
    /// tests call it with `tokio::io::duplex` halves and a `Notify`-driven
    /// interrupt closure to exercise EOF, slash commands, and SIGINT
    /// handling without touching real signals or terminals.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with<RD, W, E, I, IFut, P, PFut, C, CFut>(
        stdin: RD,
        mut stdout: W,
        mut stderr: E,
        mut interrupt: I,
        banner: &str,
        commands: &[HelpEntry],
        mut on_prompt: P,
        mut on_command: C,
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
        if !banner.is_empty() {
            stderr.write_all(banner.as_bytes()).await?;
            if !banner.ends_with('\n') {
                stderr.write_all(b"\n").await?;
            }
            stderr.flush().await?;
        }

        let help_text = compose_help(commands);
        let mut lines = stdin.lines();
        let mut last_was_interrupt = false;

        loop {
            stderr.write_all(b"> ").await?;
            stderr.flush().await?;

            let line_opt = tokio::select! {
                res = lines.next_line() => res?,
                _ = interrupt() => {
                    stderr.write_all(b"\n").await?;
                    stderr.flush().await?;
                    if last_was_interrupt {
                        return Ok(());
                    }
                    last_was_interrupt = true;
                    continue;
                }
            };

            let Some(line) = line_opt else {
                stderr.write_all(b"\n").await?;
                stderr.flush().await?;
                return Ok(());
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
                    ("help", true) => write_stderr_line(&mut stderr, &help_text).await?,
                    _ => match on_command(name.to_string(), args).await {
                        Some(text) => write_stderr_line(&mut stderr, &text).await?,
                        None => {
                            // `raw`, not name + args: the notice echoes the
                            // input as typed.
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
