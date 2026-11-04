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
//! A round also ends once the child holds nothing: before the model call that
//! would follow a work child's accepted submission, when no other waits, so a
//! round is not spent speaking after the work. A release ends the runner. The
//! round in flight is given until its next model call to end, and is dropped
//! only if a model call keeps it past `RELEASE_WAIT`.
//!
//! An agent class instance's child has one round before the rule applies: its
//! orienting round, which opens as it launches, to get ready in. Requests that
//! arrive meanwhile are announced as in any round, and its baseline is empty,
//! so whatever is still open when it ends opens the next round. Its instance's
//! `ready()` hears when it ended, or why it cannot be had.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

use super::children::{Registry, Summary};
use super::orientation;
use super::{Agent, AgentError};
use crate::harness::event::ChildKind;

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

/// Drive child `id`'s rounds until its registry, or the session, ends.
/// `instance` is the agent class instance whose child it is, if it is one.
pub(crate) async fn run(
    registry: Registry,
    id: String,
    kind: ChildKind,
    agent: Agent,
    wake: Arc<Notify>,
    instance: Option<u64>,
) {
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
    let mut wait = RETRY_FIRST;
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
        let ended = match (children.spent_budget(), &broken) {
            (Some(budget), _) => Some((
                "budget",
                format!(
                    "the session's token-budget ({budget} tokens) was spent before this was \
                     answered"
                ),
            )),
            (None, Some(why)) => Some(("failed", why.clone())),
            (None, None) => None,
        };
        if let Some((outcome, why)) = ended {
            if let Some(instance) = orienting.take() {
                children.oriented(&id, instance, Some((outcome, &why)));
            }
            if open.is_empty() {
                children.idle(&id);
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
        if orienting.is_none() && live.is_empty() {
            children.idle(&id);
            drop(children);
            wake.notified().await;
            continue;
        }
        // Unchanged since the last round opened on it, and holding something.
        let unchanged = orienting.is_none() && live == baseline;
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
        if !children.round_opened(&id, agent.history.next_round()) {
            return;
        }
        // The submission the round opens on, if it opens on one.
        let mut opened_on = None;
        // Whether this round is the warning: a submission given now is new.
        let mut warning = unchanged;
        let (lead, detail) = match kind {
            _ if orienting.is_some() => (orientation::ORIENTING.to_string(), String::new()),
            ChildKind::Request => {
                let outstanding =
                    orientation::requests(&children.unread_text(&id), &children.taken(&id));
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
                        children.round_ended(&id);
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
            agent.announcer().opened();
            let ended = agent
                .opened(|omitted| orientation::child_opening(&lead, &detail, omitted))
                .await;
            let Err(AgentError::Prompt(failed)) = &ended else {
                break ended;
            };
            failures += 1;
            if failures == ROUND_FAILURES_MAX {
                break ended;
            }
            tracing::warn!("child agent {id}'s round failed; trying again in {wait:?}: {failed}");
            back_off(&mut wait).await;
            match registry.upgrade() {
                Some(children) if !children.closed() => {
                    if !children.round_opened(&id, agent.history.next_round()) {
                        return;
                    }
                }
                _ => return,
            }
        };
        let Some(children) = registry.upgrade() else {
            return;
        };
        children.round_ended(&id);
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
                wait = RETRY_FIRST;
            }
            Err(AgentError::Closed(_)) => return,
            // The baseline and the warning stay as they were: a round that
            // failed is neither the warning nor the round after it.
            Err(AgentError::PromptAfterWork(failed)) => {
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
            // Only the last of a run of failures gets here, the others having
            // been opened again. The request was the same each time.
            Err(AgentError::Prompt(failed)) => {
                broken = Some(format!(
                    "the child's round failed {ROUND_FAILURES_MAX} times in a row on the same \
                     request, so it is not sent again: {failed}"
                ));
            }
            // Its model can answer nothing until the configuration changes,
            // so what it holds cannot be answered either.
            Err(failed) => {
                broken = Some(format!(
                    "the child's model call failed, and will not recover: {failed}"
                ));
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
