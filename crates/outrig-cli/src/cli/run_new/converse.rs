//! The terminal around a `run-new` session.
//!
//! Every typed line goes onto the agent's `user` channel, including one typed
//! while a round runs, and the model is told how many wait rather than what
//! they say. One round runs at a time: a line arriving mid-round is queued and
//! says so, and never starts a second model loop. When a round ends, the next
//! is tried at once, for whatever arrived after the round's last tool result;
//! the library runs it only if something did.
//!
//! What reaches the terminal, and where:
//!
//! - stdout carries what the agent sends on the channel, and nothing else, so
//!   `outrig run-new > out.txt` keeps the messages the agent meant the user to
//!   have.
//! - stderr carries the rest: the model's own text, which is commentary, the
//!   Python each round runs, the prompt, and notes.
//!
//! Two writers, one terminal. The rounds and the agent's sends -- one of them
//! from a task still running after its round ended -- are written by this one
//! task, in whole lines, so neither cuts into the other. The terminal also
//! paces the agent: this loop takes its messages one at a time, writing each
//! before taking the next, and the agent's code waits once it is a few messages
//! ahead of what has been taken. A send arriving at the
//! prompt ends the prompt's line and draws the prompt again after it. A line
//! the user has half typed stays in the terminal's buffer and is still read
//! whole, though its echo is split around the message.
//!
//! Stdin is read on a thread of its own rather than through tokio. A read is
//! always in flight, and tokio's blocking read would hold the runtime open at
//! exit until the user pressed Enter; a thread does not hold up the process.

use std::error::Error;
use std::future::Future;
use std::io::BufRead;
use std::pin::Pin;

use futures_util::FutureExt;
use futures_util::stream::{FuturesOrdered, StreamExt};
use outrig::PythonAgent;
use tokio::io::{AsyncWriteExt, Stderr, Stdout};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, mpsc};

use crate::error::Result;
use crate::repl::{INTERRUPT_NOTICE, compose_help, write_stderr_line};

type BoxError = Box<dyn Error + Send + Sync>;

/// How long, once input has ended, the answers to lines typed before it are
/// waited for. The interpreter's reader thread gives them at once, and a line
/// refused on the host has its answer already; only an interpreter nothing can
/// run in -- native code holding it -- takes this long.
const ANSWER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// A round in flight, borrowing the agent for as long as it runs.
type Round<'a> = Pin<Box<dyn Future<Output = std::result::Result<Option<String>, BoxError>> + 'a>>;

/// The agent, behind a lock a round holds across its awaits.
type Shared<'a> = Mutex<&'a mut PythonAgent>;

/// Run the session until the user leaves -- `/quit`, end of input, or a second
/// Ctrl-C at the prompt -- or the interpreter exits.
///
/// The agent sits behind an async mutex so a round can borrow it across its
/// awaits. A Ctrl-C while the round waits on Python goes to that Python through
/// [`PythonAgent::interrupter`], and the round carries on, since the model
/// reads how the code ended. With no Python running it drops the round's
/// future, and with it the guard, so the agent -- its conversation and its
/// interpreter -- stays with the session rather than going down with the
/// future. Nothing then starts a round until the user types again.
///
/// SIGINT is received through one listener for the whole session rather than
/// a fresh `ctrl_c()` per wait, which would miss a press landing between two
/// of them -- and a quick second press is the one that stops waiting.
///
/// The agent is borrowed, not taken, so the caller can shut it down after.
pub(super) async fn converse(agent: &mut PythonAgent) -> Result<i32> {
    let user = agent.user_channel();
    let interrupt = agent.interrupter();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut lines = read_lines()?;
    let agent = Mutex::new(agent);
    let help = compose_help(&[]);
    let mut term = Terminal {
        stdout: tokio::io::stdout(),
        stderr: tokio::io::stderr(),
        reading: true,
    };

    let mut round: Option<Round<'_>> = None;
    // Each posted line's answer, and whether it was typed while a round ran.
    let mut acks = FuturesOrdered::new();
    // Set once the interpreter has exited and everything it sent is shown.
    let mut gone = false;
    let mut last_was_interrupt = false;

    term.prompt().await?;
    loop {
        if round.is_none() {
            if gone {
                term.note("[outrig] the Python interpreter exited; the session is over")
                    .await?;
                return Ok(1);
            }
            if !term.reading {
                // Input ending is how piped input finishes, so what became of
                // the lines typed before it, and what the agent had already
                // sent, are shown first. An answer is waited for only so long,
                // and a task still sending not at all.
                let deadline = tokio::time::Instant::now() + ANSWER_GRACE;
                while let Ok(Some((queued, mid_round))) =
                    tokio::time::timeout_at(deadline, acks.next()).await
                {
                    term.answered(queued, mid_round).await?;
                }
                for text in user.receive_waiting() {
                    term.message(&text, false).await?;
                }
                return Ok(0);
            }
        }
        // Not `biased`: whichever branches are ready, one is picked at random,
        // so a steady stream of the agent's messages cannot starve the round,
        // what the user types, or anything else.
        tokio::select! {
            Some(()) = sigint.recv() => {
                if round.is_some() {
                    if let Some(said) = interrupt() {
                        term.note(&format!("\n[outrig] {said}")).await?;
                        continue;
                    }
                    round = None;
                    term.stderr.write_all(INTERRUPT_NOTICE).await?;
                } else {
                    term.stderr.write_all(b"\n").await?;
                    if last_was_interrupt {
                        return Ok(0);
                    }
                }
                last_was_interrupt = true;
                term.prompt().await?;
            }
            sent = user.receive(), if !gone => match sent {
                Some(text) => term.message(&text, round.is_none()).await?,
                None => gone = true,
            },
            ended = async { round.as_mut().expect("polled only with a round").await },
                if round.is_some() =>
            {
                round = None;
                match ended {
                    Ok(Some(reply)) => {
                        if !reply.is_empty() {
                            term.note(&reply).await?;
                        }
                        // Anything that arrived after the round's last tool
                        // result is told in a round of its own.
                        round = Some(start(&agent));
                    }
                    Ok(None) => term.prompt().await?,
                    Err(e) => {
                        // Not retried: a failure that persists would loop.
                        term.note(&format!("[outrig] error: {e}")).await?;
                        term.prompt().await?;
                    }
                }
            }
            Some((queued, mid_round)) = acks.next() => term.answered(queued, mid_round).await?,
            line = lines.recv(), if term.reading => {
                let Some(line) = line else {
                    if round.is_none() {
                        term.stderr.write_all(b"\n").await?;
                        term.stderr.flush().await?;
                    }
                    term.reading = false;
                    continue;
                };
                let line = line?;
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    if round.is_none() {
                        term.prompt().await?;
                    }
                    continue;
                }
                last_was_interrupt = false;
                if let Some(raw) = line.strip_prefix('/') {
                    match raw.split_whitespace().collect::<Vec<_>>()[..] {
                        ["quit"] => return Ok(0),
                        ["help"] => term.note(&help).await?,
                        // `raw`, as typed.
                        _ => term.note(&format!("[outrig] unknown command: /{raw}")).await?,
                    }
                    if round.is_none() {
                        term.prompt().await?;
                    }
                    continue;
                }
                // Queued before the round below asks what waits. Its answer
                // comes later, so a line is never held up behind it.
                let mid_round = round.is_some();
                acks.push_back(user.send(line).map(move |queued| (queued, mid_round)));
                if round.is_none() {
                    round = Some(start(&agent));
                }
            }
        }
    }
}

/// A round over the agent, which [`PythonAgent::round`] runs only if a message
/// the model has not been told of is waiting.
fn start<'a>(agent: &'a Shared<'_>) -> Round<'a> {
    Box::pin(async move { agent.lock().await.round().await })
}

/// Typed lines, read on a thread of their own, ending at end of input or at
/// the first read that fails.
fn read_lines() -> std::io::Result<mpsc::UnboundedReceiver<std::io::Result<String>>> {
    let (typed, lines) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("stdin".to_string())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let failed = line.is_err();
                if typed.send(line).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(lines)
}

/// The session's two output streams. Only [`converse`] writes to them.
struct Terminal {
    stdout: Stdout,
    stderr: Stderr,
    /// Whether input is still being read. Once it is not, there is no prompt
    /// to draw.
    reading: bool,
}

impl Terminal {
    async fn prompt(&mut self) -> Result<()> {
        if self.reading {
            self.stderr.write_all(b"> ").await?;
            self.stderr.flush().await?;
        }
        Ok(())
    }

    /// A line on stderr.
    async fn note(&mut self, text: &str) -> Result<()> {
        write_stderr_line(&mut self.stderr, text).await
    }

    /// What became of a typed line: whether the agent's channel took it, and
    /// if it was typed while a round ran, that it waits there.
    async fn answered(
        &mut self,
        queued: std::result::Result<usize, BoxError>,
        mid_round: bool,
    ) -> Result<()> {
        match queued {
            Ok(waiting) if mid_round => {
                self.note(&format!("[outrig] queued for the agent ({waiting} waiting)"))
                    .await
            }
            Ok(_) => Ok(()),
            Err(e) => self.note(&format!("[outrig] error: {e}")).await,
        }
    }

    /// A message the agent sent, on stdout. At the prompt it gets a line of its
    /// own, and the prompt is drawn again after it.
    async fn message(&mut self, text: &str, at_prompt: bool) -> Result<()> {
        let at_prompt = at_prompt && self.reading;
        if at_prompt {
            self.stderr.write_all(b"\n").await?;
            self.stderr.flush().await?;
        }
        // The helper takes any stream; it writes whole lines.
        write_stderr_line(&mut self.stdout, text).await?;
        if at_prompt {
            self.prompt().await?;
        }
        Ok(())
    }
}
