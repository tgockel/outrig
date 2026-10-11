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
//! The Python each round runs is shown from the session's events, through a
//! subscription of the terminal's own, as each submission starts. A round's
//! reply is written only once everything published before it has been, so a
//! submission is never shown after what the model said of it.
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
use outrig::harness::event::{Payload, Received, Subscription};
use outrig::harness::{RoundEnd, RoundOutcome, Session, SessionError, StillRunning, Tasks};
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

/// A round in flight, borrowing the session for as long as it runs.
type Round<'a> = Pin<Box<dyn Future<Output = std::result::Result<RoundOutcome, SessionError>> + 'a>>;

/// The session, behind a lock a round holds across its awaits.
type Shared<'a> = Mutex<&'a mut Session>;

/// Run the session until the user leaves -- `/quit`, end of input, or a second
/// Ctrl-C at the prompt -- or the interpreter exits. Each way the user leaves
/// closes the session to new work first; stopping it, and what the stop found,
/// is the caller's.
///
/// The session sits behind an async mutex so a round can borrow it across its
/// awaits. A Ctrl-C while the round waits on Python goes to that Python through
/// [`Session::interrupter`], and the round carries on, since the model reads
/// how the code ended. With no Python running it drops the round's future, and
/// with it the guard, so the session -- its conversation and its interpreter
/// -- stays rather than going down with the future. Nothing then starts a
/// round until the user types again. At the prompt, a Ctrl-C stops Python an
/// earlier one left holding the interpreter, through [`Session::stop_held`],
/// and counts as the first of the two that exit, so Ctrl-C Ctrl-C leaves
/// whether or not the code could be stopped.
///
/// SIGINT is received through one listener for the whole session rather than
/// a fresh `ctrl_c()` per wait, which would miss a press landing between two
/// of them -- and a quick second press is the one that stops waiting.
///
/// `shown` is the terminal's subscription to the session's events, which is
/// where the Python each round runs is shown from.
pub(super) async fn converse(session: &mut Session, shown: &mut Subscription) -> Result<()> {
    let user = session.user_channel();
    let interrupt = session.interrupter();
    let closer = session.closer();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut lines = read_lines()?;
    let session = Mutex::new(session);
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
    // Set once the session's events have all been shown.
    let mut shown_all = false;

    term.prompt().await?;
    loop {
        if round.is_none() {
            if gone {
                term.note("[outrig] the Python interpreter exited; the session is over")
                    .await?;
                return Ok(());
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
                closer.close_admission();
                return Ok(());
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
                } else if last_was_interrupt {
                    term.stderr.write_all(b"\n").await?;
                    closer.close_admission();
                    return Ok(());
                } else if let Some(said) = session.lock().await.stop_held() {
                    // No round holds the lock.
                    term.note(&format!("\n[outrig] {said} (Ctrl-C again exits)")).await?;
                } else {
                    term.stderr.write_all(b"\n").await?;
                }
                last_was_interrupt = true;
                term.prompt().await?;
            }
            sent = user.receive(), if !gone => match sent {
                Some(text) => term.message(&text, round.is_none()).await?,
                None => gone = true,
            },
            received = shown.recv(), if !shown_all => match received {
                Some(received) => term.event(received).await?,
                None => shown_all = true,
            },
            ended = async { round.as_mut().expect("polled only with a round").await },
                if round.is_some() =>
            {
                round = None;
                // Everything the round published is in the subscription by
                // now: its submissions go before what the model said of them.
                while let Ok(received) = shown.try_recv() {
                    term.event(received).await?;
                }
                match ended {
                    Ok(RoundOutcome::Ended(end)) => {
                        term.ended(&end).await?;
                        // Anything that arrived after the round's last tool
                        // result is told in a round of its own.
                        round = Some(start(&session));
                    }
                    // The interpreter's exit closed the session: the user's
                    // channel ending says so in a moment.
                    Err(SessionError::Closed(_)) => {}
                    Ok(_) => term.prompt().await?,
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
                        ["quit"] => {
                            // Closed first, so a round dropped here publishes
                            // nothing more of its own.
                            closer.close_admission();
                            drop(round.take());
                            return Ok(());
                        }
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
                    round = Some(start(&session));
                }
            }
        }
    }
}

/// A round over the session, which [`Session::round`] runs only if a message
/// the model has not been told of is waiting.
fn start<'a>(session: &'a Shared<'_>) -> Round<'a> {
    Box::pin(async move { session.lock().await.round().await })
}

/// What the terminal shows of a submission: its source, indented under a
/// heading, on stderr with everything else that is not the model's reply.
fn render_submission(source: &str) -> String {
    let mut text = String::from("[outrig] python:\n");
    for line in source.trim_end().lines() {
        text.push_str("    ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// What a round left running, as its closing line names it: the tasks, and
/// how many more there are; `None` when Python did not say.
fn tasks(running: &StillRunning) -> Option<(&[String], usize)> {
    match &running.tasks {
        Tasks::Listed { names, more, .. } => Some((names, *more)),
        _ => None,
    }
}

/// The line a round's end closes on: why a limit stopped it, and what its
/// code left running, so a prompt that returns is not taken for work done.
/// `None` when the model yielded and left nothing running.
fn closing_line(stopped: Option<&str>, running: Option<(&[String], usize)>) -> Option<String> {
    let running = match running {
        Some(([], _)) => None,
        Some((names, 0)) => Some(names.join(", ")),
        Some((names, more)) => Some(format!("{} and {more} more", names.join(", "))),
        None => Some("could not tell -- Python did not answer".to_string()),
    };
    match (stopped, running) {
        (None, None) => None,
        (Some(reason), None) => Some(format!("(round ended: {reason})")),
        (None, Some(running)) => Some(format!("(still running: {running})")),
        (Some(reason), Some(running)) => {
            Some(format!("(round ended: {reason}; still running: {running})"))
        }
    }
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

    /// What the terminal shows of one of the session's events: each
    /// submission as it starts, and a note when some were not shown.
    async fn event(&mut self, received: Received) -> Result<()> {
        match received {
            Received::Event(event) => {
                if let Payload::ExecSubmitted { source, .. } = &event.payload {
                    self.stderr
                        .write_all(render_submission(source).as_bytes())
                        .await?;
                    self.stderr.flush().await?;
                }
            }
            Received::Missed(missed) => {
                self.note(&format!(
                    "[outrig] {missed} of the session's events were not shown here; Python that \
                     ran among them is missing above"
                ))
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// How a round ended: the model's reply, the reasoning of a turn that held
    /// only that, and the line it closes on.
    async fn ended(&mut self, end: &RoundEnd) -> Result<()> {
        if !end.reply.trim().is_empty() {
            self.note(&end.reply).await?;
        }
        if let Some(reasoning) = &end.reasoning {
            // Said as `run` says it, so the two describe the same outcome alike.
            let silent = crate::llm::TurnEnd {
                reply: String::new(),
                stopped: None,
                recovered: Some(reasoning.clone()),
            };
            self.note(&silent.silent_report()).await?;
        }
        if let Some(line) = closing_line(end.stopped.as_deref(), tasks(&end.still_running)) {
            self.note(&line).await?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submission_is_shown_indented_under_its_heading() {
        assert_eq!(
            render_submission("x = 41\nprint(x)\n"),
            "[outrig] python:\n    x = 41\n    print(x)\n"
        );
    }

    /// A round's closing line says why a limit stopped it and what its code
    /// left running, and a round that yielded leaving nothing running has
    /// none.
    #[test]
    fn a_round_closes_on_what_stopped_it_and_what_still_runs() {
        let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        let (none, one, two) = (names(&[]), names(&["ci_run"]), names(&["ci_run", "review"]));
        assert_eq!(closing_line(None, Some((&none, 0))), None);
        assert_eq!(
            closing_line(Some("tool-call iteration max (8) reached"), Some((&none, 0))).as_deref(),
            Some("(round ended: tool-call iteration max (8) reached)")
        );
        assert_eq!(
            closing_line(None, Some((&one, 0))).as_deref(),
            Some("(still running: ci_run)")
        );
        assert_eq!(
            closing_line(Some("the cap"), Some((&two, 3))).as_deref(),
            Some("(round ended: the cap; still running: ci_run, review and 3 more)")
        );
        assert_eq!(
            closing_line(None, None).as_deref(),
            Some("(still running: could not tell -- Python did not answer)")
        );
    }
}
