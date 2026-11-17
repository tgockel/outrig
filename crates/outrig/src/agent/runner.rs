//! One child agent's rounds: when the host opens one, what it opens on, and
//! when it cancels what the child holds.
//!
//! A child's runner opens a round only when what the child holds open has
//! changed since the last round the runner opened on it, and holds something.
//! What it holds is a [`Summary`]: the requests waiting unread and those it took
//! and has not answered, by id. The summary a round opened on is the baseline,
//! kept once the round ends. So a child that answered anything gets the next
//! round, whatever arrived meanwhile, and a request it took and left unanswered
//! gets a round that names it.
//!
//! A round that ends with the summary as it opened, holding something, is
//! followed by one more, the warning: what is outstanding, that a round's end
//! is not a wait, and that ending this round so cancels it. When the warning
//! round ends with the summary unchanged too, the runner cancels every request
//! the summary holds, each sender told, and decides nothing more until it has
//! heard each end: a child is idle only once it holds nothing
//! (`plan/phase/0003-python/agent-classes.md`, "The child's rounds"). An end it
//! has not heard within [`SETTLE_WAIT`] it sends again. Any change to the
//! summary makes the warning available again.
//!
//! A work child holds one submission at a time, so its summary is the one it
//! took, or else the next waiting; once the one it took is cancelled, the next
//! waiting is the change that opens its round.
//!
//! A round that fails before its code ran is opened again, with what it opened
//! with, until it runs: the conversation keeps nothing of it, so its opening
//! was never read. It is the same request each time, so one that fails
//! [`ROUND_FAILURES_MAX`] times in a row is not opened again: the child's model
//! is taken to answer nothing, as if it could never. One that fails after its
//! code ran is kept, opening and all, and what that code did -- a completion, a
//! reply -- stands, so it is not opened again: after the same wait, the rule
//! decides from what is open then, against the baseline from before it. A
//! round that failed either way neither gives the warning nor counts as the
//! round after it. Once the session's token budget is spent, or a round finds
//! the child's model can never answer, no round opens: what the child holds
//! open ends instead, and so does anything that arrives later.
//!
//! So too at the child's model-request limit, which the runner checks after
//! the budget and before it compares summaries: what the child holds ends
//! `limit`, and so does what arrives while it stays there, with no round and
//! no model call. It then holds nothing, so the comparison never finds it idle
//! with work outstanding, and no warning round is spent on it; the warning is
//! left as it was. A round the limit stopped part way ended, as one the cap
//! stops does, and its baseline is set; one the warning opened that reaches
//! the limit ends what it would have cancelled `limit` instead. The child
//! stays resident, with its kernel and its history, and a reset or a raise
//! wakes it to decide from what it holds then (`plan/phase/0003-python/
//! model-policy.md`, "A child at its limit, and `0003-37`").
//!
//! A round the provider would not answer for what it carried -- a reply it
//! ended with `content_filter` or `refusal`, refused again when sent without
//! the turn's results -- is not opened again, since the same request is
//! refused the same way, and does not break the child, since another may not
//! be. No round opens, the warning included, until the requests the child
//! holds change -- one arrives, or one it holds ends -- and what it holds
//! stays open meanwhile, for its sender to cancel or to wait on. What the
//! refused round itself did with them, taking one or being given its
//! submission, is no change; one that arrived while it ran is, and opens the
//! next round at once. An orienting round so refused has ended all the same.
//!
//! What the child is doing -- a round open, a failed round it will try again
//! and when, a refusal, a model that can answer nothing more -- the runner
//! says to the registry as it moves, with what each round that ended said
//! (`stopped`, `round_ended`), for the child's owner to read.
//!
//! A round also ends once the child holds nothing: before the model call that
//! would follow a work child's accepted submission, when no other waits, so a
//! round is not spent speaking after the work. A release ends the runner. The
//! round in flight is given until its next model call to end, and is dropped
//! only if a model call keeps it past `RELEASE_WAIT`.
//!
//! An agent class instance's child -- a root among them -- may also hold
//! external channels, whose messages owe no reply. A message its model has
//! not been told of, waiting unread on one that announces its arrivals, opens
//! a round as a change of what it holds does: its opening says what waits, by
//! channel and count, and the round that announced it -- at its opening, or in
//! a result -- is the only one it earns, read or not. A round the provider
//! refused keeps only what its opening announced: a result it refused the
//! call carrying was never taken, so a message only that result told of opens
//! the next round. Nothing warns about a message or cancels it, and it never
//! makes a round the warning or the one after it.
//!
//! An agent class instance's child has one round before the rule applies: its
//! orienting round, which opens as it launches, to get ready in. Its opening
//! line announces what already waits, by channel and count, after its lead;
//! requests that arrive meanwhile are announced as in any round. Its baseline
//! is empty, so whatever is still open when it ends opens the next round. Its
//! instance's `ready()` hears when it ended, or why it cannot be had.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Notify;
use tokio::time::Instant;

use super::children::{Registry, Summary};
use super::{Agent, AgentError};
use super::{channel, orientation};
use crate::harness::RoundStop;
use crate::harness::event::{AttemptId, ChildKind};
use crate::harness::stop::{AgentState, RoundFacts};

/// How long a child waits after a round fails before the next opens. Each
/// failure in a row doubles it, up to [`RETRY_MAX`]. Short under test, where
/// the failures it waits out are the point.
const RETRY_FIRST: Duration = if cfg!(test) {
    Duration::from_millis(5)
} else {
    Duration::from_secs(2)
};
/// The longest a child waits after a failed round.
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How long an end the runner sent may go unheard before it is sent again.
/// Out of memory, the interpreter can lose the settle, or its notice of the
/// end it made, which it sends again when settled again.
const SETTLE_WAIT: Duration = Duration::from_secs(1);
/// How many rounds in a row may fail before their code ran. A round is opened
/// again with what it opened with, so the model is sent the same request each
/// time, and one that refuses it refuses it again.
pub(super) const ROUND_FAILURES_MAX: u32 = 5;

#[cfg(test)]
thread_local! {
    /// What a test holds the next runner to start on its thread by: it waits to be notified
    /// before its first round. The registry starts a runner before it answers the program that
    /// asked for the child, so no request a program makes can otherwise be waiting as it opens.
    pub(super) static HOLD: std::cell::RefCell<Option<Arc<Notify>>> =
        const { std::cell::RefCell::new(None) };

    /// What the next runner to start on its thread waits after a round fails, where it would
    /// wait [`RETRY_FIRST`]: long enough for a test to read its child unavailable before it
    /// tries again.
    pub(super) static FIRST_WAIT: std::cell::Cell<Option<Duration>> =
        const { std::cell::Cell::new(None) };

    /// The model requests the next runner to start on its thread counts as made before its
    /// first round: a count a resume would carry.
    pub(super) static SPENT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Drive child `id`'s rounds until its registry, or the session, ends.
/// `instance` is the agent class instance whose child it is, if it is one, and
/// `task` whether its instructions carry the task it begins in its orienting
/// round: a root's, from its class's contributions.
pub(crate) async fn run(
    registry: Registry,
    id: String,
    kind: ChildKind,
    agent: Agent,
    wake: Arc<Notify>,
    instance: Option<u64>,
    task: bool,
) {
    // The first wait after a failed round, and what a round that ends resets it to.
    #[cfg(test)]
    let first = FIRST_WAIT.take().unwrap_or(RETRY_FIRST);
    #[cfg(not(test))]
    let first = RETRY_FIRST;
    let mut wait = first;
    #[cfg(test)]
    if let Some(spent) = SPENT.take() {
        agent.slot().limit().seed(spent);
    }
    #[cfg(test)]
    if let Some(hold) = HOLD.with(|hold| hold.borrow_mut().take()) {
        hold.notified().await;
    }
    let mut baseline = Summary::default();
    // Whether the round that ended last on the baseline was the warning: the
    // next to find the summary unchanged cancels what it holds.
    let mut warned = false;
    // The ends this runner made and has not yet heard -- each request, its
    // outcome, why, and its cause -- and when it last sent them: the child
    // holds them until it hears, so nothing is decided on what it holds.
    let mut ending: Vec<(u64, &str, String, Option<String>)> = Vec::new();
    let mut sent: Option<Instant> = None;
    // Why the child's model can answer nothing, once a round has found so.
    let mut broken: Option<String> = None;
    // The latest round that failed after its code ran, by the submission it
    // opened on and how it failed: the cause a cancel of that submission
    // carries, and no other's.
    let mut cut: Option<(u64, String)> = None;
    // The requests the child held after a round the provider refused for what
    // it carried: no round opens while it holds those and no other, whether
    // waiting or taken.
    let mut resting: Option<BTreeSet<u64>> = None;
    // When the first of the latest run of failed rounds failed, while it lasts.
    let mut since: Option<SystemTime> = None;
    // The round opened last, by its number in the conversation: what a spent
    // budget, found between rounds, names.
    let mut round: Option<u32> = None;
    // The instance, until its child's orienting round has ended or cannot be had.
    let mut orienting = instance;
    loop {
        let Some(children) = registry.upgrade() else {
            return;
        };
        if children.closed() || children.released(&id) {
            return;
        }
        let open = children.open(&id);
        ending.retain(|(request, ..)| open.contains(request));
        if !ending.is_empty() {
            // Sent once made, and again each `SETTLE_WAIT` unheard, however
            // often the child is woken meanwhile.
            if sent.is_none_or(|at| at.elapsed() >= SETTLE_WAIT) {
                for (request, outcome, why, cause) in &ending {
                    children.settle(&id, *request, outcome, why, cause.as_deref());
                }
                sent = Some(Instant::now());
            }
            drop(children);
            // Each end heard wakes this.
            let _ = tokio::time::timeout(SETTLE_WAIT, wake.notified()).await;
            continue;
        }
        // The budget, which ends every agent's calls, before a broken model,
        // before the limit, which ends this one's until it is lifted.
        let ended = if let Some(budget) = children.spent_budget() {
            // A model that can answer nothing more was said so as the round
            // found it.
            if broken.is_none() {
                children.stopped(
                    &id,
                    AgentState::Failed {
                        class: format!("the session's token-budget ({budget} tokens) is spent"),
                        round,
                        attempt: None,
                    },
                );
            }
            Some((
                "budget",
                format!(
                    "the session's token-budget ({budget} tokens) was spent before this was \
                     answered"
                ),
            ))
        } else if let Some(why) = &broken {
            Some(("failed", why.clone()))
        } else if let Some(reached) = agent.slot().reached(round, &open) {
            // Said once per arrival at the limit, before anything it holds
            // ends, so each end reads the child at its limit. The parent's
            // exception names the child.
            if reached.first {
                children.limit_reached(&id, reached.spent, reached.limit, round);
            }
            let stop = RoundStop::ModelRequestLimit {
                spent: reached.spent,
                limit: reached.limit,
            };
            Some(("limit", stop.to_string()))
        } else {
            None
        };
        if let Some((outcome, why)) = ended {
            if let Some(instance) = orienting.take() {
                children.oriented(&id, instance, Some((outcome, &why)));
            }
            if open.is_empty() {
                // A child at its limit rests there, holding nothing, until a
                // reset or a raise wakes it.
                if outcome != "limit" {
                    children.idle(&id);
                }
                drop(children);
                wake.notified().await;
                continue;
            }
            ending = open
                .into_iter()
                .map(|request| (request, outcome, why.clone(), None))
                .collect();
            sent = None;
            continue;
        }
        let live = children.summary(&id, kind);
        // What waits, read once: whether a message is news, and, if a round
        // opens, what its opening says, which the announcer counts as told.
        let counts = children.counts(&id);
        // A message on an external channel it holds that its model has not
        // been told of: it opens a round, as a request does, and owes no reply,
        // so it never makes one the warning or the round after it. Only an
        // agent class instance's child -- a root among them -- has rounds that
        // are not its submissions'.
        let news = kind == ChildKind::Request && agent.announcer().unannounced(&counts);
        if orienting.is_none() && live.is_empty() && !news {
            children.idle(&id);
            drop(children);
            wake.notified().await;
            continue;
        }
        if resting.as_ref() == Some(&live.held()) && !news {
            drop(children);
            wake.notified().await;
            continue;
        }
        resting = None;
        // Unchanged since the last round opened on it, and holding something.
        let unchanged = orienting.is_none() && live == baseline && !news;
        if !unchanged {
            warned = false;
        } else if warned {
            // The warning round changed nothing either: what the child holds
            // is cancelled, each sender told, and the child holds it until the
            // ends are heard.
            for &request in live.unread.iter().chain(&live.taken) {
                let (what, without) = match kind {
                    ChildKind::Work => ("submission", "completing"),
                    ChildKind::Request if live.taken.contains(&request) => ("request", "answering"),
                    ChildKind::Request => ("request", "reading"),
                };
                ending.push((
                    request,
                    "cancelled",
                    format!(
                        "{what} {request} was cancelled: the child ended its rounds without \
                         {without} it"
                    ),
                    cause_of(cut.as_ref(), request).map(str::to_string),
                ));
            }
            sent = None;
            continue;
        }
        // Opened before a submission is given, so its taking names this round.
        let opened = agent.history.next_round();
        round = Some(opened);
        if !children.round_opened(&id, opened) {
            return;
        }
        // The submission the round opens on, if it opens on one.
        let mut opened_on = None;
        // Whether this round is the warning: a submission given now is new.
        let mut warning = unchanged;
        let unread = || channel::announcement(&counts).unwrap_or_default();
        let (lead, detail) = match kind {
            _ if orienting.is_some() => (orientation::orienting(task, &unread()), String::new()),
            ChildKind::Request => {
                let outstanding = orientation::requests(&unread(), &children.taken(&id));
                if warning {
                    orientation::requests_warning(&outstanding, &children.unread_on(&id))
                } else {
                    (outstanding, String::new())
                }
            }
            ChildKind::Work => match live.taken.first() {
                Some(&submission) => {
                    opened_on = Some(submission);
                    let result_type = children
                        .submission(submission)
                        .map(|given| given.result_type)
                        .unwrap_or_default();
                    if warning {
                        orientation::submission_warning(submission, &result_type)
                    } else {
                        (orientation::nudge(submission, &result_type), String::new())
                    }
                }
                // Given now, between rounds, and the round opens on whichever
                // it was: the one counted may have been cancelled meanwhile.
                None => {
                    warning = false;
                    let given = match children.assign(&id).await {
                        Some(submission) => children
                            .submission(submission)
                            .map(|given| (submission, given)),
                        None => None,
                    };
                    let Some((submission, given)) = given else {
                        children.round_ended(&id, None);
                        continue;
                    };
                    opened_on = Some(submission);
                    orientation::submission(
                        submission,
                        &given.inputs,
                        &given.result_type,
                        &given.schema,
                    )
                }
            },
        };
        drop(children);
        let mut failures = 0;
        let ended = loop {
            agent.announcer().opened_at(&counts);
            let ended = agent
                .opened(|omitted, notices| {
                    orientation::child_opening(&lead, &detail, omitted, notices)
                })
                .await;
            let Err(AgentError::Prompt(failed)) = &ended else {
                break ended;
            };
            failures += 1;
            if failures == ROUND_FAILURES_MAX {
                break ended;
            }
            tracing::warn!("child agent {id}'s round failed; trying again in {wait:?}: {failed}");
            match registry.upgrade() {
                Some(children) => {
                    children.stopped(&id, unavailable(&agent, failed, opened, &mut since, wait))
                }
                None => return,
            }
            back_off(&mut wait).await;
            match registry.upgrade() {
                Some(children) if !children.closed() => {
                    // Nothing of the round was kept, so its number is the same.
                    if !children.round_opened(&id, opened) {
                        return;
                    }
                }
                _ => return,
            }
        };
        let Some(children) = registry.upgrade() else {
            return;
        };
        let facts = ended.as_ref().ok().map(|finished| RoundFacts {
            round: opened,
            reply: finished.reply.clone(),
            stopped: finished.stopped.clone(),
            reasoning: finished.reasoning.clone(),
            sends: children.sends_in(&id, opened),
        });
        children.round_ended(&id, facts);
        // A release ended the round, or let it end, so what it held is no
        // longer the runner's to decide: an orienting round that ended so
        // is not ready.
        if children.released(&id) {
            return;
        }
        match ended {
            Ok(_) => {
                // What it was told of in results, such as a binding added, it read.
                agent.announcer().keep();
                // An orienting round's baseline stays empty, so what is open as
                // it ends opens the next round.
                match orienting.take() {
                    Some(instance) => children.oriented(&id, instance, None),
                    None => {
                        baseline = live;
                        warned = warning;
                    }
                }
                wait = first;
                since = None;
            }
            Err(AgentError::Closed(_)) => return,
            // Not opened again, and the child rests on what it holds until
            // that changes; what an orienting round ran, or did not, it has
            // ended all the same.
            Err(AgentError::Refused(failed)) => {
                // What its opening told of opens no round again: the same
                // opening would be refused the same way. What only a result
                // told of the model never took, so it may open the next.
                agent.announcer().settle(&counts);
                let attempt = agent.slot().last_recorded().map(|(attempt, _)| attempt);
                children.stopped(
                    &id,
                    AgentState::Refused {
                        round: opened,
                        attempt,
                    },
                );
                if let Some(instance) = orienting.take() {
                    children.oriented(&id, instance, None);
                }
                tracing::warn!(
                    "child agent {id}'s round was refused for what it carried; it waits for \
                     what it holds to change: {failed}"
                );
                // What the round did with what it held is no change; a request
                // that arrived while it ran is one, so the next round opens.
                let held = children.summary(&id, kind).held();
                if held.is_subset(&live.held()) {
                    resting = Some(held);
                }
                baseline = live;
                wait = first;
                since = None;
            }
            // The baseline and the warning stay as they were: a round that
            // failed is neither the warning nor the round after it.
            Err(AgentError::PromptAfterWork(failed)) => {
                children.stopped(&id, unavailable(&agent, &failed, opened, &mut since, wait));
                // Its code ran, so an orienting round has ended all the same.
                if let Some(instance) = orienting.take() {
                    children.oriented(&id, instance, None);
                }
                drop(children);
                tracing::warn!(
                    "child agent {id}'s round failed after its code ran; deciding again in \
                     {wait:?}: {failed}"
                );
                cut = opened_on.map(|submission| (submission, failed));
                back_off(&mut wait).await;
            }
            // Its model can answer nothing more: the last of a run of failures
            // before its code ran -- the others having been opened again, and
            // the request the same each time -- or one that does not clear
            // until the configuration changes, or one that was no model
            // call's. What it holds cannot be answered either. The terminal
            // forms' own advice -- fix the cause, then start a new session --
            // is for the person at the session rather than for the parent
            // reading why its child stopped, so those are carried by their
            // cause alone.
            Err(failed) => {
                children.stopped(&id, broken_by(&agent, &failed, opened));
                broken = Some(match failed {
                    AgentError::Prompt(cause) => format!(
                        "the child's round failed {ROUND_FAILURES_MAX} times in a row on the \
                         same request, so it is not sent again: {cause}"
                    ),
                    AgentError::PromptTerminal(cause)
                    | AgentError::PromptAfterWorkTerminal(cause) => {
                        format!("the child's model call failed, and will not recover: {cause}")
                    }
                    other => {
                        format!("the child's model call failed, and will not recover: {other}")
                    }
                });
            }
        }
    }
}

/// What a cancel of `submission` carries of `cut`, the latest round that
/// failed on a model call after its code ran: that round's failure, if it
/// opened on `submission`. A submission taken late, after the runner gave up
/// waiting for it to be given, is read by the next round without passing the
/// place a round opens on a new one, so the failure of an earlier submission's
/// round is not cleared before it, and is never this one's.
fn cause_of(cut: Option<&(u64, String)>, submission: u64) -> Option<&str> {
    cut.filter(|(of, _)| *of == submission)
        .map(|(_, failure)| failure.as_str())
}

/// Why `agent`'s latest model call failed, as its last request's
/// `model.attempt` says, whichever of an alias's models it went to, or
/// `failed`, the round's error, when it says none; and that request.
fn failure(agent: &Agent, failed: &str) -> (String, Option<AttemptId>) {
    match agent.slot().last_recorded() {
        Some((attempt, error)) => (error.unwrap_or_else(|| failed.to_string()), Some(attempt)),
        None => (failed.to_string(), None),
    }
}

/// What a child whose model can answer nothing more, once its round `round`
/// failed with `failed`, is: failed, with the last request of a model call
/// that failed, and the error itself for anything else.
fn broken_by(agent: &Agent, failed: &AgentError, round: u32) -> AgentState {
    let (class, attempt) = match failed {
        AgentError::Prompt(cause)
        | AgentError::PromptTerminal(cause)
        | AgentError::PromptAfterWorkTerminal(cause) => failure(agent, cause),
        other => (other.to_string(), None),
    };
    AgentState::Failed {
        class,
        round: Some(round),
        attempt,
    }
}

/// What a child whose round `round` failed with `failed`, in a way that may
/// clear, is: unavailable until it tries again, `wait` from now. `since` is
/// when the first of this run of failures failed, which this is when it is
/// unset.
fn unavailable(
    agent: &Agent,
    failed: &str,
    round: u32,
    since: &mut Option<SystemTime>,
    wait: Duration,
) -> AgentState {
    let now = SystemTime::now();
    let (class, attempt) = failure(agent, failed);
    AgentState::Unavailable {
        class,
        since: *since.get_or_insert(now),
        next_attempt: now + wait,
        round,
        attempt,
    }
}

/// Wait `wait`, and double it for the failure after, up to [`RETRY_MAX`].
async fn back_off(wait: &mut Duration) {
    tokio::time::sleep(*wait).await;
    *wait = (*wait * 2).min(RETRY_MAX);
}

#[cfg(test)]
mod tests {
    use super::cause_of;

    /// A round's failure is the cause of a cancel of the submission it opened on only: one
    /// left by an earlier submission's round is not carried to the next, however that next one
    /// came to be taken.
    #[test]
    fn a_failed_rounds_cause_is_its_own_submissions() {
        let cut = (3, "finish reason content_filter".to_string());
        assert_eq!(
            cause_of(Some(&cut), 3),
            Some("finish reason content_filter")
        );
        assert_eq!(cause_of(Some(&cut), 4), None);
        assert_eq!(cause_of(None, 3), None);
    }
}
