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
//! Two sources, one terminal. The rounds and the agent's sends -- one of them
//! from a task still running after its round ended -- are written in whole
//! lines, so neither cuts into the other. This loop writes nothing itself: it
//! queues, and [`Output`]'s threads write, so a terminal that is slow or has
//! stopped delays what is shown, and not the round, a Ctrl-C or what the user
//! types. stdout and stderr have a thread each, and stderr is not held up
//! behind messages stdout has yet to show. The terminal still paces the agent:
//! the loop takes a message only once stdout's queue has room for it, and the
//! agent's code waits once it is a few messages ahead of what has been taken.
//! A send arriving at the prompt ends the prompt's line and draws the prompt
//! again after it. A line the user has half typed stays in the terminal's
//! buffer and is still read whole, though its echo is split around the
//! message.
//!
//! A typed `/name` naming one of the session's skills is a directive: it goes
//! onto the channel naming the skill, and asks the agent to run it. `/help` and
//! `/quit` stay this terminal's own whatever the skills are called. No other
//! line is taken until the directive is on the channel, and a round starts
//! once it is, so a line typed after it cannot reach the agent first.
//!
//! Stdin is read on a thread of its own rather than through tokio. A read is
//! always in flight, and tokio's blocking read would hold the runtime open at
//! exit until the user pressed Enter; a thread does not hold up the process.
//!
//! On a terminal the loop is the same and only what it shows changes: the
//! [`Screen`] draws the conversation, reads what is typed, and turns Ctrl-C,
//! which raw mode makes a key rather than a signal, into the same interrupt.
//! Everything above about stdout and stderr is then the line mode's, which
//! `run-new` keeps whenever the screen is not used.

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::future::Future;
use std::io::BufRead;
use std::pin::Pin;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesOrdered, FuturesUnordered, StreamExt};
use futures_util::{FutureExt, Stream};
use outrig::harness::event::{AgentRequestId, Payload, Received, Subscription};
use outrig::harness::{
    Question, RoundEnd, RoundOutcome, RoundStop, Session, SessionError, Settle, SettleError,
    StillRunning, Tasks, Waits,
};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Mutex, mpsc};

use super::output::{self, Failures, Output, PROMPT};
use super::screen::{self, InputEvent, Screen, Typed};
use crate::cli::signals::{EndSignal, Signals};
use crate::error::Result;
use crate::repl::{HelpEntry, INTERRUPT_NOTICE, compose_help};

type BoxError = Box<dyn Error + Send + Sync>;

/// What `/help` says of a directive, when the session has skills.
const DIRECTIVE_HELP: HelpEntry = HelpEntry {
    syntax: "/<skill> [text]",
    description: "ask the agent to run one of the session's skills",
};

/// How long, once input has ended, the answers to lines typed before it are
/// waited for. The interpreter's reader thread gives them at once, and a line
/// refused on the host has its answer already; only an interpreter nothing can
/// run in -- native code holding it -- takes this long.
const ANSWER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The terminal's independent input streams: events may be lossy, questions never are.
pub(super) struct Updates<'a> {
    pub(super) questions: &'a mut mpsc::UnboundedReceiver<outrig::harness::QuestionNotice>,
    pub(super) events: &'a mut Subscription,
}

/// A round in flight, borrowing the session for as long as it runs.
type Round<'a> =
    Pin<Box<dyn Future<Output = std::result::Result<RoundOutcome, SessionError>> + 'a>>;

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
/// Every Ctrl-C comes through `interrupts`, which says why it is one
/// listener.
///
/// `view` is where everything is shown, and is the caller's: what it says as
/// the session closes goes through it too, queued behind what this showed, so
/// it is written in turn and not while a write this left stuck holds the
/// destination. End of input and the interpreter's exit show everything the
/// agent sent first, for as long as the terminal takes. `/quit` and a second
/// Ctrl-C do not wait for a stdout that may have stopped, nor, when stderr is
/// the same destination, for a stderr that waits behind it, so the few
/// messages taken but not yet written are dropped; so are they when the loop
/// fails. A Ctrl-C ends a wait for the terminal. A write that fails while the
/// session ends, as input does with stdout stopped and its reader gone, fails
/// the session as one made while it runs does.
///
/// `shown` is the terminal's subscription to the session's events, which is
/// where the Python each round runs is shown from. `typed` is what the user
/// types, and `failures` the writes `view` could not make.
///
/// `signals` is how the session is ended from outside: a SIGTERM or a SIGHUP
/// leaves as `/quit` does, and is returned so the caller can say so, finalize
/// the record with the signal's status, and exit with it. SIGINT is not one of
/// them -- at this terminal Ctrl-C is the conversation's own, and `interrupts`
/// carries it. A signal that lands while what the session showed is still being
/// written ends that wait as a Ctrl-C does, and is returned all the same.
pub(super) async fn converse(
    updates: Updates<'_>,
    session: &mut Session,
    view: Frontend<'_>,
    typed: Typed,
    interrupts: &mut Interrupts,
    failures: &mut Failures,
    signals: &mut Signals,
) -> Conversed {
    let mut term = Terminal {
        view,
        reading: true,
        waits: session.waits(),
    };
    let left = drive(
        updates, session, &mut term, typed, interrupts, failures, signals,
    )
    .await;
    let (mut signal, result) = match left {
        Ok(signal) => (signal, Ok(())),
        Err(e) => (None, Err(e)),
    };
    if signal.is_some() || result.is_err() {
        // A failure does not wait on a stdout that may have stopped either,
        // and neither does a signal: it asked for the session to end now. So
        // nothing is settled: with stdout and stderr two pipes, what stderr
        // had queued is waited for by `settle` even once stdout is abandoned,
        // and a reader that has stopped never lets it be.
        term.abandon();
    }
    if signal.is_none() {
        signal = settle(&term, interrupts, signals).await;
    }
    if signal.is_none() && screen::signalled() {
        // A signal stopped the screen, and its typed lines ended with it: the
        // loop can have read that as input ending, or the settle as done,
        // before it heard the signal. It is on its way -- the screen's handler
        // passes it on -- so it is waited for, and for as long as the wait is
        // bounded when it is not one of these.
        signal = tokio::time::timeout(SIGNAL_GRACE, signals.termination())
            .await
            .ok();
    }
    Conversed { signal, result }.reconciled(failures)
}

/// How long a signal that stopped the screen is waited for, to reach the
/// listener the conversation holds.
const SIGNAL_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How the conversation ended. The signal that ended it is kept apart from
/// whether it went well: a write that failed as the session closed is the
/// session's error, and not a reason for it to leave with any status but the
/// signal's.
pub(super) struct Conversed {
    /// The SIGTERM or SIGHUP the session was ended by, if one did.
    pub(super) signal: Option<EndSignal>,
    pub(super) result: Result<()>,
}

impl Conversed {
    /// A conversation that failed before it began.
    pub(super) fn failed(error: crate::error::CliError) -> Self {
        Self {
            signal: None,
            result: Err(error),
        }
    }

    /// `self`, with its result failed if a write has, as [`reconcile`] says.
    pub(super) fn reconciled(self, failures: &mut Failures) -> Self {
        Self {
            result: reconcile(self.result, failures),
            ..self
        }
    }
}

/// [`Terminal::settle`], ended by a Ctrl-C, or by a SIGTERM or SIGHUP, which
/// is returned: the wait is for output that has nobody to be shown to once
/// the session is asked to end, and it is not one to hold that up.
async fn settle(
    term: &Terminal<'_>,
    interrupts: &mut Interrupts,
    signals: &mut Signals,
) -> Option<EndSignal> {
    let mut signalled = None;
    term.settle(async {
        tokio::select! {
            _ = interrupts.recv() => {}
            sig = signals.termination() => signalled = Some(sig),
        }
    })
    .await;
    signalled
}

/// Where the conversation is shown: plain lines on stdout and stderr, or the
/// screen.
#[derive(Clone, Copy)]
pub(super) enum Frontend<'a> {
    Lines(&'a Output),
    Screen(&'a Screen),
}

impl<'a> Frontend<'a> {
    /// Room for one more of the agent's messages; see [`Output::room`].
    async fn room(self) -> Option<Room<'a>> {
        match self {
            Frontend::Lines(out) => out.room().await.map(Room::Lines),
            Frontend::Screen(screen) => screen.room().await.map(Room::Screen),
        }
    }
}

/// Each Ctrl-C: SIGINT, as a terminal in canonical mode sends it, and with the
/// screen up, the key itself, which raw mode reads instead.
///
/// One listener for the whole session rather than a fresh `ctrl_c()` per
/// wait, which would miss a press landing between two of them -- and a quick
/// second press is the one that stops waiting. It is made before the screen
/// takes the terminal, so a SIGINT from elsewhere meets tokio's handler and
/// never its default action, which would end the process with the terminal
/// still raw.
pub(super) struct Interrupts {
    sigint: Option<Signal>,
    keys: Option<mpsc::UnboundedReceiver<()>>,
}

impl Interrupts {
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            sigint: Some(signal(SignalKind::interrupt())?),
            keys: None,
        })
    }

    /// Ctrl-C pressed at the screen, too.
    pub(super) fn listen(&mut self, keys: mpsc::UnboundedReceiver<()>) {
        self.keys = Some(keys);
    }

    /// The next Ctrl-C. Completes for nothing else: every caller reads a
    /// completion as a press, so once the screen's keys have ended, SIGINT is
    /// the one listened to.
    pub(super) async fn recv(&mut self) -> Option<()> {
        let Self { sigint, keys } = self;
        tokio::select! {
            Some(()) = async { sigint.as_mut()?.recv().await } => Some(()),
            Some(()) = async { keys.as_mut()?.recv().await } => Some(()),
            else => std::future::pending().await,
        }
    }
}

/// `left`, unless it is a success and a write has failed: that left output
/// short of what the session was told, so the session did not end cleanly. It
/// takes the writes that have failed by now, and none that are yet to.
pub(super) fn reconcile(left: Result<()>, failures: &mut Failures) -> Result<()> {
    match failures.try_recv() {
        Ok(failed) if left.is_ok() => Err(failed.into()),
        _ => left,
    }
}

/// The loop [`converse`] runs. It returns with everything it showed queued,
/// and stdout abandoned if the user asked to go; writing the rest is
/// [`converse`]'s to wait for. A SIGTERM or SIGHUP is returned as the signal
/// it was.
async fn drive(
    updates: Updates<'_>,
    session: &mut Session,
    term: &mut Terminal<'_>,
    mut lines: Typed,
    interrupts: &mut Interrupts,
    failures: &mut Failures,
    signals: &mut Signals,
) -> Result<Option<EndSignal>> {
    let Updates {
        questions,
        events: shown,
    } = updates;
    // `run-new` always starts its session with a primary agent.
    let (Some(user), Some(interrupt)) = (session.user_channel(), session.interrupter()) else {
        return Err(SessionError::NoPrimary.into());
    };
    let question_channel = session
        .channel(outrig::harness::QUESTION_CHANNEL)
        .expect("built-in question channel");
    let mut questions_state = Questions::default();
    let closer = session.closer();
    let view = term.view;
    let session = Mutex::new(session);
    // Read before any line is, so a `/name` typed at once finds its skill.
    let skills = read_catalog(shown, term);
    let mut help = compose_help(if skills.is_empty() {
        &[]
    } else {
        &[DIRECTIVE_HELP]
    });

    help.push_str("\n/answer <id> <numbers and/or text | --decline [reason] | --not-relevant>\n//answer always invokes the built-in, even with an answer skill.\n");

    let mut round: Option<Round<'_>> = None;
    // Each posted line's answer, and whether it was typed while a round ran.
    let mut acks = FuturesOrdered::new();
    // Answer validation runs on the asker's kernel. A held kernel must not hold
    // up input, output, or interrupts while its settlement is acknowledged.
    let mut answers = FuturesUnordered::new();
    // A directive's answer, and whether it was typed while a round ran: no
    // line is taken until it comes.
    let mut directing: Option<BoxFuture<'static, (std::result::Result<usize, BoxError>, bool)>> =
        None;
    // Set once the interpreter has exited and everything it sent is taken.
    let mut gone = false;
    let mut last_was_interrupt = false;
    // Set once the session's events have all been shown.
    let mut shown_all = false;

    term.prompt();
    loop {
        if round.is_none() {
            if gone {
                // What the interpreter sent is shown before this.
                if let Some(sig) = settle(term, interrupts, signals).await {
                    return Ok(Some(sig));
                }
                term.note("[outrig] the Python interpreter exited; the session is over");
                return Ok(None);
            }
            if !term.reading {
                // Input ending is how piped input finishes, so what became of
                // the lines typed before it, and what the agent had already
                // sent, are shown first. An answer is waited for only so long,
                // and a task still sending not at all.
                let deadline = tokio::time::Instant::now() + ANSWER_GRACE;
                drain_receipts(&mut acks, &mut answers, deadline, |receipt| match receipt {
                    Receipt::Message(queued, mid_round) => term.answered(queued, mid_round),
                    Receipt::Answer(receipt) => receipt.apply(&mut questions_state, view),
                })
                .await;
                let signalled = tokio::select! {
                    () = show(view, user.receive_waiting()) => None,
                    _ = interrupts.recv() => {
                        term.abandon();
                        None
                    }
                    sig = signals.termination() => {
                        term.abandon();
                        Some(sig)
                    }
                };
                closer.close_admission();
                return Ok(signalled);
            }
        }
        // Not `biased`: whichever branches are ready, one is picked at random,
        // so a steady stream of the agent's messages cannot starve the round,
        // what the user types, or anything else.
        tokio::select! {
            // A signal from outside ends the session, as `/quit` does: the
            // round in flight is dropped, nothing more is admitted, and the
            // stop and the report follow. What it was going to show is not
            // waited for -- whoever signalled is not waiting to read it.
            sig = signals.termination() => {
                closer.close_admission();
                term.abandon();
                drop(round.take());
                return Ok(Some(sig));
            }
            Some(()) = interrupts.recv() => {
                if round.is_some() {
                    if let Some(said) = interrupt() {
                        term.note(&format!("\n[outrig] {said}"));
                        continue;
                    }
                    round = None;
                    term.interrupted();
                } else if last_was_interrupt {
                    term.newline();
                    closer.close_admission();
                    term.abandon();
                    return Ok(None);
                } else if let Some(said) = session.lock().await.stop_held() {
                    // No round holds the lock.
                    term.note(&format!("\n[outrig] {said} (Ctrl-C again exits)"));
                } else {
                    term.newline();
                }
                last_was_interrupt = true;
                term.prompt();
            }
            Some(notice) = questions.recv() => {
                use outrig::harness::QuestionNotice;
                match notice {
                    QuestionNotice::Asked(item) => {
                        if let (Some(id), Ok(question)) = (item.request, serde_json::from_value::<outrig::harness::Question>(item.body)) {
                            let asker = if item.chain.is_empty() { item.sender } else { item.chain.join(" -> ") };
                            match view {
                                Frontend::Screen(screen) => {
                                    screen.question_entry(format!("Question #{} · {}", id, asker), format_question_body(&question));
                                    screen.question(id, asker, question.clone());
                                }
                                Frontend::Lines(_) => {
                                    term.note(&format!("[outrig] question from {asker}"));
                                    term.note(&format_question(id.get(), &question));
                                }
                            }
                            questions_state.ids.insert(id.get(), id);
                            questions_state.open.insert(id.get(), question);
                        }
                    }
                    QuestionNotice::Withdrawn(notice) => {
                        questions_state.withdrawn(notice.request.get(), notice.end, view);
                    }
                    QuestionNotice::Settled(notice)
                        if !matches!(notice.outcome, outrig::harness::Settlement::Refused { .. }) =>
                    {
                        if !questions_state.finish(notice.request.get()) { continue; }
                        match view {
                            Frontend::Screen(screen) => {
                                screen.question_closed(notice.request.get());
                                screen.question_entry(format!("Question #{} settled", notice.request), format_settlement(&notice.outcome));
                            }
                            Frontend::Lines(_) => term.note(&format!("[outrig.question.closed] {}", notice.request)),
                        }
                    }
                    _ => {}
                }
            }
            Some(failed) = failures.recv() => return Err(failed.into()),
            // A message is taken only with room made for it, so a terminal
            // slower than the agent leaves the rest with the agent, whose code
            // waits, and leaves this loop free.
            Some((room, sent)) = async {
                let room = view.room().await?;
                Some((room, user.receive().await))
            }, if !gone => match sent {
                Some(text) => room.send(text, round.is_none() && term.reading),
                None => gone = true,
            },
            received = shown.recv(), if !shown_all => match received {
                Some(received) => term.event(received),
                None => shown_all = true,
            },
            ended = async { round.as_mut().expect("polled only with a round").await },
                if round.is_some() =>
            {
                round = None;
                // Everything the round published is in the subscription by
                // now: its submissions go before what the model said of them.
                while let Ok(received) = shown.try_recv() {
                    term.event(received);
                }
                match ended {
                    Ok(RoundOutcome::Ended(end)) => {
                        term.ended(&end);
                        if asks_again(end.stopped.as_ref()) {
                            // Anything that arrived after the round's last tool
                            // result is told in a round of its own.
                            round = Some(start(&session));
                        } else {
                            term.prompt();
                        }
                    }
                    // The interpreter's exit closed the session: the user's
                    // channel ending says so in a moment.
                    Err(SessionError::Closed(_)) => {}
                    Ok(_) => term.prompt(),
                    Err(e) => {
                        // Not retried: a failure that persists would loop.
                        term.error(&format!("[outrig] error: {e}"));
                        term.prompt();
                    }
                }
            }
            Some(receipt) = answers.next() => {
                receipt.apply(&mut questions_state, view);
                if round.is_none() { round = Some(start(&session)); }
            }
            Some((queued, mid_round)) = acks.next() => term.answered(queued, mid_round),
            (queued, mid_round) = async {
                directing.as_mut().expect("polled only while directing").await
            }, if directing.is_some() => {
                directing = None;
                term.answered(queued, mid_round);
                // On the channel by now, unless it was refused: a round tells
                // the agent, or finds nothing new and shows the prompt.
                if round.is_none() {
                    round = Some(start(&session));
                }
            }
            line = lines.recv(), if term.reading && directing.is_none() => {
                let Some(line) = line else {
                    if round.is_none() {
                        term.newline();
                    }
                    term.reading = false;
                    continue;
                };
                let input = line?;
                if input.is_activity() { last_was_interrupt = false; }
                let line = match input {
                    InputEvent::Line(line) => line,
                    InputEvent::Answer(id, settle) => {
                        if let Some(request) = questions_state.ids.get(&id) {
                            answers.push(settle_answer(&question_channel, *request, settle));
                        }
                        if round.is_none() { round = Some(start(&session)); }
                        continue;
                    }
                };
                let line = line.trim_end_matches('\r');
                if line.is_empty() {
                    if round.is_none() {
                        term.prompt();
                    }
                    continue;
                }
                let built_in = answer_command(line, skills.contains("answer"));
                if let Some(raw) = built_in {
                    let result = parse_answer(raw, &questions_state);
                    match result {
                        Ok((id, settle)) => {
                            answers.push(settle_answer(&question_channel, id, settle));
                        }
                        Err(e) => term.note(&format!("[outrig] {e}")),
                    }
                    if round.is_none() { round = Some(start(&session)); }
                    continue;
                }
                if let Some(raw) = line.strip_prefix('/') {
                    match raw.split_whitespace().collect::<Vec<_>>()[..] {
                        ["quit"] => {
                            // Closed first, so a round dropped here publishes
                            // nothing more of its own.
                            closer.close_admission();
                            term.abandon();
                            drop(round.take());
                            return Ok(None);
                        }
                        ["help"] => term.note(&help),
                        // The name follows the slash, and whitespace ends it: the
                        // text the agent is told to take is what comes after.
                        [name, ..] if !raw.starts_with(char::is_whitespace)
                            && skills.contains(name) =>
                        {
                            let mid_round = round.is_some();
                            directing = Some(
                                user.send_directive(name, line)
                                    .map(move |queued| (queued, mid_round))
                                    .boxed(),
                            );
                            continue;
                        }
                        // `raw`, as typed.
                        _ => term.note(&format!("[outrig] unknown command: /{raw}")),
                    }
                    if round.is_none() {
                        term.prompt();
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

/// The names of the skills a `/name` runs, from the `skill.discovered` event
/// the session queued in `shown` as it started, showing each event taken as
/// the loop would. A skill named for a built-in command is said, and left out.
fn read_catalog(shown: &mut Subscription, term: &Terminal<'_>) -> HashSet<String> {
    let mut skills = HashSet::new();
    while let Ok(received) = shown.try_recv() {
        if let Received::Event(event) = &received
            && let Payload::SkillDiscovered { skills: found, .. } = &event.payload
        {
            skills.extend(found.iter().map(|skill| skill.name.clone()));
        }
        term.event(received);
    }
    for name in ["help", "quit"] {
        if skills.remove(name) {
            term.note(&format!(
                "[outrig] skill {name:?} is not run by /{name}, which is run-new's own command; \
                 the agent can still use it through outrig.skills"
            ));
        }
    }
    skills
}

/// Queue `messages` to be shown, each as room is made for it.
async fn show(view: Frontend<'_>, messages: Vec<String>) {
    if let Frontend::Screen(screen) = view {
        screen.draining();
    }
    for text in messages {
        let Some(room) = view.room().await else {
            return;
        };
        room.send(text, false);
    }
}

/// Whether a round that ended `stopped` is followed at once by another, for
/// what arrived as it ran: not once the agent's model-request limit stopped
/// it, since at the limit a round opens on nothing and what waits stays
/// unread, so asking again would only say the limit again. The next line
/// typed asks.
fn asks_again(stopped: Option<&RoundStop>) -> bool {
    !matches!(stopped, Some(RoundStop::ModelRequestLimit { .. }))
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

/// What a round's code left running, as its closing line names it; `None`
/// when it left nothing.
fn still_running(running: Option<(&[String], usize)>) -> Option<String> {
    match running {
        Some(([], _)) => None,
        Some((names, more)) => Some(screen::joined(names, more as u64)),
        None => Some("could not tell -- Python did not answer".to_string()),
    }
}

/// The line a round's end closes on: why a limit stopped it, and what its
/// code left running, so a prompt that returns is not taken for work done.
/// `None` when the model yielded and left nothing running.
fn closing_line(stopped: Option<&str>, running: Option<(&[String], usize)>) -> Option<String> {
    match (stopped, still_running(running)) {
        (None, None) => None,
        (Some(reason), None) => Some(format!("(round ended: {reason})")),
        (None, Some(running)) => Some(format!("(still running: {running})")),
        (Some(reason), Some(running)) => {
            Some(format!("(round ended: {reason}; still running: {running})"))
        }
    }
}

/// Typed lines, read on a thread of their own, ending at end of input or at
/// the first read that fails. A line that is not UTF-8 is still a line: read
/// as bytes and decoded lossily, as `outrig run`'s prompt reads one, rather
/// than ending input the way `BufRead::lines` would.
pub(super) fn read_lines() -> std::io::Result<Typed> {
    let (typed, lines) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name("stdin".to_string())
        .spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut bytes = Vec::new();
            loop {
                bytes.clear();
                let line = match stdin.read_until(b'\n', &mut bytes) {
                    Ok(0) => return,
                    Ok(_) => Ok(InputEvent::Line(decode_line(&bytes))),
                    Err(e) => Err(e),
                };
                let failed = line.is_err();
                if typed.send(line).is_err() || failed {
                    return;
                }
            }
        })?;
    Ok(lines)
}

/// One line as `read_until` returns it, as `BufRead::lines` would give it --
/// without its `\n` or `\r\n` -- but with U+FFFD for each sequence that is not
/// UTF-8.
fn decode_line(bytes: &[u8]) -> String {
    let line = match bytes.strip_suffix(b"\n") {
        Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
        None => bytes,
    };
    String::from_utf8_lossy(line).into_owned()
}

/// The subject of the main agent's events (`doc/reference/events.md`).
pub(super) const MAIN_AGENT: &str = "agent/primary";

/// What the session shows, handed to whichever frontend shows it: rendered
/// and queued on [`Output`], which writes it, or handed to the [`Screen`].
/// Nothing here waits on the terminal.
struct Terminal<'a> {
    view: Frontend<'a>,
    /// Whether input is still being read. Once it is not, there is no prompt
    /// to draw.
    reading: bool,
    /// What lists the waits the session's agents are in, for the screen
    /// after a gap in the events.
    waits: Waits,
}

/// A place for one of the agent's messages, in whichever frontend shows it.
enum Room<'a> {
    Lines(output::Room<'a>),
    Screen(screen::Room<'a>),
}

impl Room<'_> {
    /// `framed` is the line mode's: whether the message came while the prompt
    /// showed. The screen draws its prompt apart from the conversation.
    fn send(self, text: String, framed: bool) {
        match self {
            Room::Lines(room) => room.send(text, framed),
            Room::Screen(room) => room.send(text),
        }
    }
}

impl Terminal<'_> {
    fn prompt(&self) {
        if let Frontend::Lines(out) = self.view
            && self.reading
        {
            out.text(PROMPT);
        }
    }

    /// A line about the session: on stderr, or in the conversation.
    fn note(&self, text: &str) {
        match self.view {
            Frontend::Lines(out) => out.line(text),
            Frontend::Screen(screen) => screen.note(text),
        }
    }

    fn error(&self, text: &str) {
        match self.view {
            Frontend::Lines(_) => self.note(text),
            Frontend::Screen(screen) => screen.error(text),
        }
    }

    /// A Ctrl-C dropped the round.
    fn interrupted(&self) {
        match self.view {
            Frontend::Lines(out) => out.text(INTERRUPT_NOTICE),
            Frontend::Screen(screen) => screen.note(&String::from_utf8_lossy(INTERRUPT_NOTICE)),
        }
    }

    /// End the line a Ctrl-C or the end of input left the prompt on. The
    /// screen's prompt is not on a line of the conversation.
    fn newline(&self) {
        if let Frontend::Lines(out) = self.view {
            out.text("\n");
        }
    }

    fn abandon(&self) {
        match self.view {
            Frontend::Lines(out) => out.abandon(),
            Frontend::Screen(screen) => screen.abandon(),
        }
    }

    /// Wait for what is queued to be shown; see [`Output::settle`].
    async fn settle(&self, interrupted: impl Future) {
        match self.view {
            Frontend::Lines(out) => out.settle(interrupted).await,
            Frontend::Screen(screen) => screen.settle(interrupted).await,
        }
    }

    /// What the terminal shows of one of the session's events. The line mode
    /// shows each of the main agent's submissions as it starts, what the
    /// screen would put in the conversation, and a note when some were not
    /// shown; a child's code runs under the child's own subject, and is left
    /// to the event log. The screen reads them all.
    fn event(&self, received: Received) {
        let out = match self.view {
            Frontend::Lines(out) => out,
            Frontend::Screen(screen) => {
                screen.event(&received);
                // The gap may have hidden a wait starting or ending: what is
                // open now is read back, which only the screen shows.
                if let Received::Missed(_) = received {
                    screen.waiting(self.waits.open());
                }
                return;
            }
        };
        if let Some(line) = screen::conversation_line(&received) {
            self.note(&line);
        }
        match received {
            Received::Event(event) => {
                if let Payload::ExecSubmitted { source, .. } = &event.payload
                    && event.subject.as_ref().map(|s| s.as_str()) == Some(MAIN_AGENT)
                {
                    out.text(render_submission(source));
                }
            }
            Received::Missed(missed) => {
                self.note(&format!(
                    "[outrig] {missed} of the session's events were not shown here; Python that \
                     ran among them is missing above"
                ));
            }
            _ => {}
        }
    }

    /// How a round ended: the model's reply, the reasoning of a turn that held
    /// only that, and the line it closes on.
    fn ended(&self, end: &RoundEnd) {
        let reasoning = end.reasoning.as_ref().map(|reasoning| {
            // Said as `run` says it, so the two describe the same outcome alike.
            let silent = crate::llm::TurnEnd {
                reply: String::new(),
                stopped: None,
                recovered: Some(reasoning.clone()),
            };
            silent.silent_report()
        });
        let running = tasks(&end.still_running);
        let stopped = end.stopped.as_ref().map(ToString::to_string);
        let closing = closing_line(stopped.as_deref(), running);
        match self.view {
            Frontend::Lines(_) => {
                if !end.reply.trim().is_empty() {
                    self.note(&end.reply);
                }
                if let Some(reasoning) = &reasoning {
                    self.note(reasoning);
                }
                if let Some(line) = &closing {
                    self.note(line);
                }
            }
            Frontend::Screen(screen) => {
                screen.ended(reasoning, closing, still_running(running));
            }
        }
    }

    /// What became of a typed line: whether the agent's channel took it, and
    /// if it was typed while a round ran, that it waits there -- which the
    /// screen keeps for its detail view, its status line counting them.
    fn answered(&self, queued: std::result::Result<usize, BoxError>, mid_round: bool) {
        let waiting = match queued {
            Ok(waiting) => waiting,
            Err(e) => return self.error(&format!("[outrig] error: {e}")),
        };
        if let Frontend::Screen(screen) = self.view {
            screen.queued(waiting);
        }
        if mid_round {
            let note = format!(
                "[outrig] queued; the agent has {waiting} {} unread",
                crate::cli::session_setup::plural(waiting, "message", "messages")
            );
            match self.view {
                Frontend::Lines(out) => out.line(&note),
                Frontend::Screen(screen) => screen.detail(&note),
            }
        }
    }
}

/// Keep bodies only while open. Compact IDs let late answers reach the
/// backend's authoritative refusal path without retaining prompts or context.
#[derive(Default)]
struct Questions {
    open: BTreeMap<u64, Question>,
    ids: BTreeMap<u64, AgentRequestId>,
    /// Which authoritative outcomes have been shown, never provisional cancel requests.
    reported: HashSet<u64>,
    /// Cancellation is a request to withdraw, not the kernel's final outcome.
    cancellation_requested: HashSet<u64>,
}

impl Questions {
    fn finish(&mut self, id: u64) -> bool {
        self.close(id);
        self.cancellation_requested.remove(&id);
        self.reported.insert(id)
    }

    fn withdrawn(&mut self, id: u64, end: outrig::harness::WithdrawEnd, view: Frontend<'_>) {
        self.close(id);
        if self.reported.contains(&id) {
            return;
        }
        let cancelled = matches!(end, outrig::harness::WithdrawEnd::Cancelled);
        // A queued owner answer can still win on the kernel. Remove the modal
        // now, but record only the cancellation request, never a final outcome
        // that would suppress that answer's authoritative receipt. This also
        // holds if the keyboard's answer has yet to reach the conversation loop.
        if cancelled {
            if !self.cancellation_requested.insert(id) {
                return;
            }
        } else if !self.finish(id) {
            return;
        }
        match view {
            Frontend::Screen(screen) => {
                screen.question_closed(id);
                if cancelled {
                    screen.question_entry(
                        format!("Question #{id} cancellation requested"),
                        "Asking agent requested cancellation".into(),
                    );
                } else {
                    screen.question_entry(
                        format!("Question #{id} closed"),
                        "Asking agent released or session closed".into(),
                    );
                }
            }
            Frontend::Lines(out) => out.line(&format!("[outrig.question.closed] {id}")),
        }
    }

    fn close(&mut self, id: u64) {
        self.open.remove(&id);
    }

    fn answered(
        &mut self,
        id: u64,
        answer: std::result::Result<(), SettleError>,
    ) -> Option<String> {
        if answer.is_ok()
            || matches!(
                answer,
                Err(SettleError::Ended(_) | SettleError::Rejected(_) | SettleError::Gone)
            )
        {
            self.close(id);
        }
        answer.err().map(|error| error.to_string())
    }
}

/// Owner settlements acknowledge directly; they do not echo a Settled notice.
/// Retain what was submitted until that acknowledgement, including during EOF drain.
struct AnswerReceipt {
    id: u64,
    submitted: Settle,
    result: std::result::Result<(), SettleError>,
}

async fn settle_answer(
    channel: &outrig::harness::Channel,
    id: AgentRequestId,
    submitted: Settle,
) -> AnswerReceipt {
    let result = channel.settle(id, submitted.clone()).await;
    AnswerReceipt {
        id: id.get(),
        submitted,
        result,
    }
}

impl AnswerReceipt {
    fn apply(self, state: &mut Questions, view: Frontend<'_>) {
        let Self {
            id,
            submitted,
            result,
        } = self;
        let accepted = result.is_ok();
        let error = state.answered(id, result);
        if let Some(error) = error {
            let text = format!("[outrig] {error}");
            match view {
                Frontend::Lines(out) => out.line(&text),
                Frontend::Screen(screen) => screen.note(&text),
            }
        }
        if accepted {
            if state.finish(id)
                && let Frontend::Screen(screen) = view
            {
                screen.question_closed(id);
                let text = match submitted {
                    Settle::Reply(value) => {
                        format_settlement(&outrig::harness::Settlement::Replied(value))
                    }
                    Settle::Fail(reason) => {
                        format_settlement(&outrig::harness::Settlement::Failed(reason))
                    }
                    Settle::Cancel(reason) => format!("Cancelled: {reason}"),
                    _ => format!("{submitted:?}"),
                };
                screen.question_entry(format!("Question #{id} settled"), text);
            }
        } else if let Frontend::Screen(screen) = view {
            if state.open.contains_key(&id) {
                screen.question_retry(id);
            } else {
                screen.question_closed(id);
            }
        }
    }
}

enum Receipt {
    Message(std::result::Result<usize, BoxError>, bool),
    Answer(AnswerReceipt),
}

/// EOF owes diagnostics for both kinds of input, under one shared deadline.
async fn drain_receipts<A, B>(
    acks: &mut A,
    answers: &mut B,
    deadline: tokio::time::Instant,
    mut received: impl FnMut(Receipt),
) where
    A: Stream<Item = (std::result::Result<usize, BoxError>, bool)> + Unpin,
    B: Stream<Item = AnswerReceipt> + Unpin,
{
    loop {
        let next = async {
            tokio::select! {
                Some((queued, mid_round)) = acks.next() => Some(Receipt::Message(queued, mid_round)),
                Some(receipt) = answers.next() => Some(Receipt::Answer(receipt)),
                else => None,
            }
        };
        match tokio::time::timeout_at(deadline, next).await {
            Ok(Some(receipt)) => received(receipt),
            _ => break,
        }
    }
}

fn question_text(text: &str) -> String {
    serde_json::to_string(text)
        .unwrap()
        .chars()
        .map(|c| {
            if c.is_control() {
                format!("\\u{:04x}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// JSON strings keep untrusted text on one physical line without terminal escapes.
fn format_question(id: u64, q: &outrig::harness::Question) -> String {
    let mut lines = vec![format!("[outrig.question] {id} {}", question_text(&q.text))];
    for (n, c) in q.choices.iter().enumerate() {
        lines.push(format!(
            "[outrig.choice] {id} {} {} {}",
            n + 1,
            question_text(&c.label),
            question_text(&c.description)
        ));
    }
    if let Some(context) = &q.context {
        lines.push(format!(
            "[outrig.context] {id} {}",
            question_text(&context.description)
        ));
        for reference in &context.references {
            let body = serde_json::to_value(reference).expect("reference JSON");
            // Re-escape C1 controls as well as JSON's mandatory C0 escaping.
            let body: String = body
                .to_string()
                .chars()
                .map(|c| {
                    if c.is_control() {
                        format!("\\u{:04x}", c as u32)
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            lines.push(format!("[outrig.reference] {id} {body}"));
        }
    }
    lines.join("\n")
}

fn format_question_body(q: &Question) -> String {
    let mut text = q.text.clone();
    for (n, c) in q.choices.iter().enumerate() {
        text.push_str(&format!("\n{}. {} -- {}", n + 1, c.label, c.description));
    }
    if let Some(context) = &q.context {
        text.push_str(&format!("\n{}", context.description));
        for r in &context.references {
            let at = r.line.map_or(String::new(), |n| format!(":{n}"));
            let end = r.end_line.map_or(String::new(), |n| format!("-{n}"));
            text.push_str(&format!("\n{}{at}{end} {}", r.path, r.description));
        }
    }
    text
}

fn format_settlement(outcome: &outrig::harness::Settlement) -> String {
    use outrig::harness::Settlement;
    match outcome {
        Settlement::Replied(value) => {
            match serde_json::from_value::<outrig::harness::Answer>(value.clone()) {
                Ok(answer) => format!(
                    "Answered: {}{}",
                    if answer.choices.is_empty() && answer.text.is_none() {
                        "(no selections)".into()
                    } else {
                        answer.choices.join(", ")
                    },
                    answer.text.map_or(String::new(), |t| format!(" · {t}"))
                ),
                Err(_) => format!("Answered: {value}"),
            }
        }
        Settlement::Failed(reason) => format!("Declined: {reason}"),
        Settlement::Cancelled { reason, .. } => format!("Cancelled: {reason}"),
        _ => format!("{outcome:?}"),
    }
}

fn answer_command(line: &str, skill: bool) -> Option<&str> {
    let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    match command {
        "//answer" => Some(rest),
        "/answer" if !skill => Some(rest),
        _ => None,
    }
}

fn parse_answer(
    raw: &str,
    questions: &Questions,
) -> std::result::Result<(AgentRequestId, Settle), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("usage: /answer <id> <answer>".into());
    }
    let (id, rest) = raw.split_once(char::is_whitespace).unwrap_or((raw, ""));
    let id: u64 = id.parse().map_err(|_| "invalid question id")?;
    let request = *questions.ids.get(&id).ok_or("no question with that id")?;
    let Some(question) = questions.open.get(&id) else {
        // Keep only IDs after closure. The backend refuses this late settlement
        // before reading its body, preserving its lifecycle event and diagnostic.
        return Ok((request, Settle::Fail("late answer".into())));
    };
    Ok((request, answer_value(rest, question)?))
}

fn answer_value(
    rest: &str,
    question: &outrig::harness::Question,
) -> std::result::Result<outrig::harness::Settle, String> {
    use outrig::harness::{Answer, Settle};
    let rest = rest.trim();
    if rest == "--not-relevant" {
        return Ok(Settle::Fail("not relevant".into()));
    }
    if rest == "--decline" || rest.starts_with("--decline ") {
        return Ok(Settle::Fail(
            rest.strip_prefix("--decline").unwrap().trim().to_string(),
        ));
    }
    let (first, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let mut choices = Vec::new();
    let text = if !question.choices.is_empty()
        && !first.is_empty()
        && first.chars().all(|c| c.is_ascii_digit() || c == ',')
    {
        for n in first.split(',') {
            let n: usize = n.parse().map_err(|_| "invalid choice number")?;
            let choice = n
                .checked_sub(1)
                .and_then(|n| question.choices.get(n))
                .ok_or("choice number out of range")?;
            choices.push(choice.label.clone());
        }
        (!tail.trim().is_empty()).then(|| tail.trim().to_string())
    } else {
        (!rest.is_empty()).then(|| rest.to_string())
    };
    let answer = Answer {
        choices,
        text,
        by: "user".into(),
        chain: Vec::new(),
    };
    Ok(Settle::Reply(serde_json::to_value(answer).unwrap()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A key pressed at the screen is a Ctrl-C, as SIGINT is. The screen's
    /// keys ending is not one: every caller reads a completion as a press, so
    /// once they end the wait goes on for SIGINT alone.
    #[tokio::test]
    async fn a_key_is_an_interrupt_and_the_keys_ending_is_not() {
        let (press, keys) = mpsc::unbounded_channel();
        let mut interrupts = Interrupts {
            sigint: None,
            keys: Some(keys),
        };
        press.send(()).expect("pressed");
        assert_eq!(interrupts.recv().await, Some(()));
        drop(press);
        let waited = tokio::time::timeout(Duration::from_millis(100), interrupts.recv()).await;
        assert!(waited.is_err(), "the keys ending completed the wait");
    }

    /// A byte that is not UTF-8 costs only itself: the line is kept, with
    /// U+FFFD in its place, and line endings go as `lines` drops them.
    #[test]
    fn a_line_that_is_not_utf8_is_decoded_lossily() {
        assert_eq!(decode_line(b"caf\xe9 au lait\n"), "caf\u{fffd} au lait");
        assert_eq!(decode_line(b"crlf\r\n"), "crlf");
        assert_eq!(decode_line(b"no newline at end"), "no newline at end");
        assert_eq!(decode_line(b"\n"), "");
    }

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
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        let (none, one, two) = (names(&[]), names(&["ci_run"]), names(&["ci_run", "review"]));
        assert_eq!(closing_line(None, Some((&none, 0))), None);
        assert_eq!(
            closing_line(
                Some("tool-call iteration max (8) reached"),
                Some((&none, 0))
            )
            .as_deref(),
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

    /// A round another stop ended is followed by one for what arrived as it ran; the stops a
    /// round of the session's can end with otherwise are covered by `run_new_e2e`.
    #[test]
    fn a_round_a_release_ended_asks_again() {
        assert!(asks_again(None));
        assert!(asks_again(Some(&RoundStop::Released)));
    }
}

#[cfg(test)]
mod question_tests {
    use super::*;
    use outrig::harness::{Choice, Question};
    fn question() -> Question {
        Question {
            text: "pick\n\u{1b}[31m\u{9b}".into(),
            choices: vec![
                Choice {
                    label: "a".into(),
                    description: "first".into(),
                },
                Choice {
                    label: "b".into(),
                    description: "second".into(),
                },
            ],
            multiple: true,
            other: true,
            context: None,
        }
    }
    #[test]
    fn questions_have_stable_safe_physical_lines() {
        assert_eq!(
            format_question(42, &question()),
            "[outrig.question] 42 \"pick\\n\\u001b[31m\\u009b\"\n[outrig.choice] 42 1 \"a\" \"first\"\n[outrig.choice] 42 2 \"b\" \"second\""
        );
    }

    #[test]
    fn question_context_and_file_references_are_safe_inert_data() {
        use outrig::harness::{QuestionContext, QuestionReference};
        let mut q = question();
        q.context = Some(QuestionContext {
            description: "longer\nbackground".into(),
            references: vec![QuestionReference {
                path: "src/parser.rs".into(),
                line: Some(12),
                end_line: Some(18),
                description: "parser\u{1b}".into(),
            }],
        });
        let shown = format_question(42, &q);
        assert!(
            shown.contains("[outrig.context] 42 \"longer\\nbackground\""),
            "{shown}"
        );
        assert!(shown.contains("[outrig.reference] 42 {"), "{shown}");
        assert!(shown.contains("src/parser.rs"), "{shown}");
        assert!(!shown.contains('\u{1b}'), "{shown}");
    }

    #[test]
    fn screen_entries_show_answers_declines_and_empty_selections_literally() {
        use outrig::harness::{Answer, Settlement};
        let empty = serde_json::to_value(Answer {
            choices: Vec::new(),
            text: None,
            by: "user".into(),
            chain: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            format_settlement(&Settlement::Replied(empty)),
            "Answered: (no selections)"
        );
        assert_eq!(
            format_settlement(&Settlement::Failed("not relevant".into())),
            "Declined: not relevant"
        );
        assert_eq!(
            format_settlement(&Settlement::Failed(" ask later ".into())),
            "Declined:  ask later "
        );
        let reply = serde_json::to_value(Answer {
            choices: vec!["one".into()],
            text: Some("typed 123".into()),
            by: "user".into(),
            chain: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            format_settlement(&Settlement::Replied(reply)),
            "Answered: one · typed 123"
        );
        assert!(format_question_body(&question()).contains("1. a -- first"));
    }

    #[test]
    fn answers_are_explicit_and_a_skill_can_shadow_the_command() {
        assert_eq!(answer_command("ordinary reply", false), None);
        assert_eq!(answer_command("/answer 42 1,2", false), Some("42 1,2"));
        assert_eq!(answer_command("/answer 42 1,2", true), None);
        assert_eq!(answer_command("//answer 42 1,2", true), Some("42 1,2"));
        assert_eq!(answer_command("/answer", false), Some(""));
        assert_eq!(answer_command("/answering 42", false), None);
    }
    #[test]
    fn numbered_text_and_decline_answers_parse() {
        use outrig::harness::Settle;
        let q = question();
        for (input, choices, text) in [
            ("", vec![], None),
            ("2", vec!["b"], None),
            ("1,2 and also this", vec!["a", "b"], Some("and also this")),
            ("free text", vec![], Some("free text")),
        ] {
            let Settle::Reply(value) = answer_value(input, &q).unwrap() else {
                panic!("not a reply");
            };
            assert_eq!(value["choices"], serde_json::json!(choices));
            assert_eq!(value["text"], serde_json::json!(text));
        }
        assert_eq!(
            answer_value("--not-relevant", &q),
            Ok(Settle::Fail("not relevant".into()))
        );
        assert_eq!(
            answer_value("--decline not now", &q),
            Ok(Settle::Fail("not now".into()))
        );
        assert_eq!(
            answer_value("--decline", &q),
            Ok(Settle::Fail(String::new()))
        );
        assert!(answer_value("3", &q).is_err());
    }

    #[test]
    fn completed_questions_drop_bodies_but_invalid_answers_keep_them() {
        let mut state = Questions::default();
        for id in 0..1000 {
            state.open.insert(id, question());
            assert!(state.answered(id, Ok(())).is_none());
            assert!(state.open.is_empty());
            state.open.insert(id, question());
            state.close(id); // Withdrawal or terminal backend notice.
            assert!(state.open.is_empty());
        }
        state.open.insert(42, question());
        assert!(
            state
                .answered(42, Err(SettleError::InvalidAnswer("duplicate".into())))
                .is_some()
        );
        assert!(state.open.contains_key(&42));
    }

    #[test]
    fn successful_owner_receipts_close_and_record_once_without_a_backend_notice() {
        // Both modal submissions and explicit /answer use this receipt path.
        // No QuestionNotice::Settled follows a successful owner settlement.
        for raw in [
            "1,2 typed text",
            "",
            "--decline ask later",
            "--not-relevant",
        ] {
            let (screen, mut updates) = Screen::capture_questions();
            let mut state = Questions::default();
            state.open.insert(42, question());
            state.open.insert(43, question());
            let submitted = answer_value(raw, &question()).unwrap();
            AnswerReceipt {
                id: 42,
                submitted: submitted.clone(),
                result: Ok(()),
            }
            .apply(&mut state, Frontend::Screen(&screen));
            assert!(!state.open.contains_key(&42));
            assert_eq!(state.open.len(), 1);
            let shown = updates();
            assert_eq!(shown.len(), 2, "{shown:?}");
            assert_eq!(shown[0], "closed:42");
            let expected = match &submitted {
                Settle::Reply(value) => {
                    format_settlement(&outrig::harness::Settlement::Replied(value.clone()))
                }
                Settle::Fail(reason) => format!("Declined: {reason}"),
                _ => unreachable!(),
            };
            assert_eq!(shown[1], format!("Question #42 settled:{expected}"));
            // Repeated receipts or a late terminal notice cannot duplicate an entry.
            AnswerReceipt {
                id: 42,
                submitted,
                result: Ok(()),
            }
            .apply(&mut state, Frontend::Screen(&screen));
            assert!(updates().is_empty());
            assert!(!state.finish(42));
        }
    }

    #[test]
    fn provisional_withdrawals_never_hide_an_accepted_owner_answer_in_either_order() {
        use outrig::harness::WithdrawEnd;
        for withdrawal_first in [false, true] {
            for raw in ["1 typed answer", "--decline not now"] {
                let (screen, mut updates) = Screen::capture_questions();
                let mut state = Questions::default();
                state.open.insert(42, question());
                if withdrawal_first {
                    state.withdrawn(42, WithdrawEnd::Cancelled, Frontend::Screen(&screen));
                    assert!(state.open.is_empty(), "remove the dialog immediately");
                    assert!(
                        !state.reported.contains(&42),
                        "a withdrawal is not a final outcome"
                    );
                    assert_eq!(
                        updates(),
                        [
                            "closed:42",
                            "Question #42 cancellation requested:Asking agent requested cancellation"
                        ]
                    );
                    // A repeat cannot duplicate the provisional note either.
                    state.withdrawn(42, WithdrawEnd::Cancelled, Frontend::Screen(&screen));
                    assert!(updates().is_empty());
                }
                // The kernel accepts the owner settlement despite the withdrawal.
                // This also covers input whose receipt starts after notice delivery.
                let submitted = answer_value(raw, &question()).unwrap();
                AnswerReceipt {
                    id: 42,
                    submitted,
                    result: Ok(()),
                }
                .apply(&mut state, Frontend::Screen(&screen));
                let shown = updates();
                assert_eq!(shown.len(), 2);
                assert_eq!(shown[0], "closed:42");
                assert!(shown[1].starts_with("Question #42 settled:"));
                assert!(shown[1].contains(if raw.starts_with("--decline") {
                    "Declined: not now"
                } else {
                    "Answered: a · typed answer"
                }));
                assert!(state.reported.contains(&42));
                assert!(!state.cancellation_requested.contains(&42));
                state.withdrawn(42, WithdrawEnd::Cancelled, Frontend::Screen(&screen));
                assert!(
                    updates().is_empty(),
                    "a late withdrawal cannot contradict the receipt"
                );
            }
        }
    }

    #[test]
    fn a_cancellation_request_without_an_owner_answer_is_removed_but_not_called_final() {
        use outrig::harness::WithdrawEnd;
        let (screen, mut updates) = Screen::capture_questions();
        let mut state = Questions::default();
        state.open.insert(42, question());
        state.withdrawn(42, WithdrawEnd::Cancelled, Frontend::Screen(&screen));
        assert!(state.open.is_empty());
        assert!(!state.reported.contains(&42));
        assert_eq!(
            updates(),
            [
                "closed:42",
                "Question #42 cancellation requested:Asking agent requested cancellation"
            ]
        );
        // Release, unlike a cancel request, reports a request that already ended.
        state.withdrawn(42, WithdrawEnd::Released, Frontend::Screen(&screen));
        assert!(state.reported.contains(&42));
        assert_eq!(
            updates(),
            [
                "closed:42",
                "Question #42 closed:Asking agent released or session closed"
            ]
        );
    }

    #[test]
    fn invalid_owner_receipts_retry_without_an_answer_entry_and_can_later_succeed() {
        let (screen, mut updates) = Screen::capture_questions();
        let mut state = Questions::default();
        state.open.insert(42, question());
        AnswerReceipt {
            id: 42,
            submitted: Settle::Reply(serde_json::json!({})),
            result: Err(SettleError::InvalidAnswer("missing answer".into())),
        }
        .apply(&mut state, Frontend::Screen(&screen));
        assert!(state.open.contains_key(&42));
        assert!(!state.reported.contains(&42));
        assert_eq!(
            updates(),
            [":[outrig] invalid answer: missing answer", "retry:42",]
        );
        AnswerReceipt {
            id: 42,
            submitted: answer_value("1", &question()).unwrap(),
            result: Ok(()),
        }
        .apply(&mut state, Frontend::Screen(&screen));
        assert!(state.open.is_empty());
        assert_eq!(updates().len(), 2);
    }

    #[tokio::test]
    async fn eof_drain_reconciles_successful_answers_too() {
        use futures_util::stream;
        let (screen, mut updates) = Screen::capture_questions();
        let mut state = Questions::default();
        state.open.insert(42, question());
        let mut acks = stream::empty();
        let mut answers = stream::iter([AnswerReceipt {
            id: 42,
            submitted: Settle::Fail("not relevant".into()),
            result: Ok(()),
        }]);
        drain_receipts(
            &mut acks,
            &mut answers,
            tokio::time::Instant::now() + ANSWER_GRACE,
            |receipt| match receipt {
                Receipt::Answer(receipt) => receipt.apply(&mut state, Frontend::Screen(&screen)),
                _ => panic!("no user messages"),
            },
        )
        .await;
        assert!(state.open.is_empty());
        assert_eq!(
            updates(),
            ["closed:42", "Question #42 settled:Declined: not relevant"]
        );
    }

    #[tokio::test]
    async fn eof_drains_a_final_answer_refusal_and_message_receipts() {
        use futures_util::stream;
        let mut state = Questions::default();
        state.open.insert(42, question());
        let mut acks = stream::iter([(Ok(0), false)]);
        let mut answers = stream::iter([AnswerReceipt {
            id: 42,
            submitted: Settle::Fail("not used".into()),
            result: Err(SettleError::InvalidAnswer("duplicate selections".into())),
        }]);
        let mut errors = Vec::new();
        let mut messages = 0;
        drain_receipts(
            &mut acks,
            &mut answers,
            tokio::time::Instant::now() + ANSWER_GRACE,
            |receipt| match receipt {
                Receipt::Message(_, _) => messages += 1,
                Receipt::Answer(receipt) => {
                    errors.extend(state.answered(receipt.id, receipt.result))
                }
            },
        )
        .await;
        assert_eq!(messages, 1);
        assert_eq!(errors, ["invalid answer: duplicate selections"]);
        assert!(state.open.contains_key(&42));
    }

    #[tokio::test]
    async fn eof_receipt_drain_still_has_a_deadline() {
        use futures_util::stream;
        let mut acks = stream::empty::<(std::result::Result<usize, BoxError>, bool)>();
        let mut answers = stream::pending::<AnswerReceipt>();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            drain_receipts(&mut acks, &mut answers, deadline, |_| panic!("no receipts")),
        )
        .await
        .expect("bounded EOF drain");
    }
}
