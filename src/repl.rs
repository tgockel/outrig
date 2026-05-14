//! Interactive stdin/stdout REPL with slash commands.
//!
//! `Repl::run` drives the I/O loop: print a banner on stderr, prompt with `> `,
//! and feed each non-slash line to a caller-supplied async callback. Slash
//! commands (`/help`, `/quit`, `/tools`, `/reset`) are handled here; `/tools`
//! and `/reset` defer to caller-supplied callbacks for their text + side
//! effects (history clearing, tool-list assembly). EOF (Ctrl-D) and `/quit`
//! exit cleanly. SIGINT during a callback cancels the in-flight future,
//! prints `[outrig] interrupted`, and returns to the prompt; a second
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

const INTERRUPT_NOTICE: &[u8] = b"\n[outrig] interrupted\n";

pub struct Repl;

impl Repl {
    /// Run the REPL against real stdin/stdout/stderr, treating
    /// `tokio::signal::ctrl_c()` as the interrupt source. The `banner` is
    /// printed once to stderr before the first prompt; `on_prompt` is invoked
    /// for every non-slash, non-empty input line and its non-empty returned
    /// text is printed to stdout. Streaming callers may write incrementally
    /// during the callback and return an empty string to suppress trailing
    /// reprint. `on_tools` and `on_reset` produce the stderr text for `/tools`
    /// and `/reset` respectively (and `on_reset` is the side-effect site for
    /// clearing whatever conversation state the caller owns).
    pub async fn run<P, PFut, T, TFut, R, RFut>(
        banner: &str,
        on_prompt: P,
        on_tools: T,
        on_reset: R,
    ) -> Result<()>
    where
        P: FnMut(String) -> PFut,
        PFut: Future<Output = Result<String>>,
        T: FnMut() -> TFut,
        TFut: Future<Output = String>,
        R: FnMut() -> RFut,
        RFut: Future<Output = String>,
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
            on_prompt,
            on_tools,
            on_reset,
        )
        .await
    }

    /// Generic form parameterized over the I/O streams and interrupt source.
    /// Production calls this with real handles via [`Repl::run`]; integration
    /// tests call it with `tokio::io::duplex` halves and a `Notify`-driven
    /// interrupt closure to exercise EOF, slash commands, and SIGINT
    /// handling without touching real signals or terminals.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_with<RD, W, E, I, IFut, P, PFut, T, TFut, R, RFut>(
        stdin: RD,
        mut stdout: W,
        mut stderr: E,
        mut interrupt: I,
        banner: &str,
        mut on_prompt: P,
        mut on_tools: T,
        mut on_reset: R,
    ) -> Result<()>
    where
        RD: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
        E: AsyncWrite + Unpin,
        I: FnMut() -> IFut,
        IFut: Future<Output = ()>,
        P: FnMut(String) -> PFut,
        PFut: Future<Output = Result<String>>,
        T: FnMut() -> TFut,
        TFut: Future<Output = String>,
        R: FnMut() -> RFut,
        RFut: Future<Output = String>,
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
                        write_stderr_line(&mut stderr, &on_tools().await).await?;
                    }
                    "reset" => {
                        write_stderr_line(&mut stderr, &on_reset().await).await?;
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
