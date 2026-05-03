//! Interactive stdin/stdout REPL with slash commands.
//!
//! `Repl::run` drives the I/O loop: print a banner on stderr, prompt with `> `,
//! and feed each non-slash line to a caller-supplied async callback. Slash
//! commands (`/help`, `/quit`, `/tools`, `/reset`) are handled here; the latter
//! two print placeholders until 0019 wires real data through. EOF (Ctrl-D)
//! and `/quit` exit cleanly. SIGINT during a callback cancels the in-flight
//! future, prints `[outrig] interrupted`, and returns to the prompt; a second
//! consecutive SIGINT (no input typed in between) exits.
//!
//! Strict stream separation: assistant text goes to stdout; everything else --
//! banner, prompt, slash-command output, interrupt notice -- goes to stderr.
//! That way `outrig run > out.txt` captures only the model's replies.
//!
//! [`Repl::run_with`] is the generic form: it takes any `AsyncBufRead` /
//! `AsyncWrite` streams plus an interrupt-future factory, so integration tests
//! can substitute `tokio::io::duplex` halves and a `tokio::sync::Notify`-driven
//! interrupt source.

use std::future::Future;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::error::Result;

const HELP_TEXT: &str = "\
[outrig] slash commands:
  /help    show this help
  /tools   list registered tools
  /reset   clear conversation history
  /quit    exit the session
";

const TOOLS_PLACEHOLDER: &[u8] = b"[outrig] (no tools registered)\n";
const RESET_PLACEHOLDER: &[u8] = b"[outrig] (no history to reset)\n";
const INTERRUPT_NOTICE: &[u8] = b"\n[outrig] interrupted\n";

pub struct Repl;

impl Repl {
    /// Run the REPL against real stdin/stdout/stderr, treating
    /// `tokio::signal::ctrl_c()` as the interrupt source. The `banner` is
    /// printed once to stderr before the first prompt; `on_prompt` is invoked
    /// for every non-slash, non-empty input line and its returned text is
    /// printed to stdout.
    pub async fn run<F, Fut>(banner: &str, on_prompt: F) -> Result<()>
    where
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let stdin = BufReader::new(tokio::io::stdin());
        let stdout = tokio::io::stdout();
        let stderr = tokio::io::stderr();
        Self::run_with(stdin, stdout, stderr, ctrl_c_signal, banner, on_prompt).await
    }

    /// Generic form parameterized over the I/O streams and interrupt source.
    /// Production calls this with real handles via [`Repl::run`]; integration
    /// tests call it with `tokio::io::duplex` halves and a `Notify`-driven
    /// interrupt closure to exercise EOF, slash commands, and SIGINT
    /// handling without touching real signals or terminals.
    pub async fn run_with<R, W, E, I, IFut, F, Fut>(
        stdin: R,
        mut stdout: W,
        mut stderr: E,
        mut interrupt: I,
        banner: &str,
        mut on_prompt: F,
    ) -> Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
        E: AsyncWrite + Unpin,
        I: FnMut() -> IFut,
        IFut: Future<Output = ()>,
        F: FnMut(String) -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        if !banner.is_empty() {
            stderr.write_all(banner.as_bytes()).await?;
            if !banner.ends_with('\n') {
                stderr.write_all(b"\n").await?;
            }
            stderr.flush().await?;
        }

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

            if let Some(cmd) = trimmed.strip_prefix('/') {
                match cmd {
                    "quit" => return Ok(()),
                    "help" => {
                        stderr.write_all(HELP_TEXT.as_bytes()).await?;
                        stderr.flush().await?;
                    }
                    "tools" => {
                        stderr.write_all(TOOLS_PLACEHOLDER).await?;
                        stderr.flush().await?;
                    }
                    "reset" => {
                        stderr.write_all(RESET_PLACEHOLDER).await?;
                        stderr.flush().await?;
                    }
                    other => {
                        stderr
                            .write_all(format!("[outrig] unknown command: /{other}\n").as_bytes())
                            .await?;
                        stderr.flush().await?;
                    }
                }
                continue;
            }

            tokio::select! {
                res = on_prompt(trimmed.to_string()) => {
                    let reply = res?;
                    stdout.write_all(reply.as_bytes()).await?;
                    if !reply.ends_with('\n') {
                        stdout.write_all(b"\n").await?;
                    }
                    stdout.flush().await?;
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
