//! One child agent's rounds: when the host opens one, and what it opens on.
//!
//! A child's runner opens a round only when what the child holds open has
//! changed since the last round the runner opened on it, and holds something.
//! What it holds is a [`Summary`]: the requests waiting unread and those it took
//! and has not answered, by id. The summary a round opened on is the baseline,
//! kept once the round ends. So a child that answered anything gets the next
//! round, whatever arrived meanwhile; a request it took and left unanswered
//! earns one further round; one it never took earns none past the first that
//! counted it; and a round that ends with nothing changed is followed by
//! nothing until an arrival, a cancel or a release changes it
//! (`plan/phase/0003-python/agent-classes.md`, "The child's rounds").
//!
//! A work child holds one submission at a time, so its summary is the one it
//! took, or else the next waiting. Where the rule would leave a taken
//! submission open and the child idle -- its round ended without completing it,
//! and so did the one more round that asked for it -- the submission ends
//! rejected instead, and the next waiting is the change that opens its round.
//!
//! A round that fails before its code ran is opened again, with what it opened
//! with, until it runs: the conversation keeps nothing of it, so its opening
//! was never read. It is the same request each time, so one that fails
//! [`ROUND_FAILURES_MAX`] times in a row is not opened again: the child's model
//! is taken to answer nothing, as if it could never. One that fails after its
//! code ran is kept, opening and all, and what that code did -- a completion, a
//! reply -- stands, so it is not opened again: after the same wait, the rule
//! decides from what is open then, against the baseline from before it. Once
//! the session's token budget is spent, or a round finds the child's model can
//! never answer, no round opens: what the child holds open ends instead, and
//! so does anything that arrives later.
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

use tokio::sync::{Notify, watch};

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
/// How many rounds in a row may fail before their code ran. A round is opened
/// again with what it opened with, so the model is sent the same request each
/// time, and one that refuses it refuses it again.
pub(super) const ROUND_FAILURES_MAX: u32 = 5;

/// Drive child `id`'s rounds until its registry, or the session, ends.
/// `spent` says when the session's token budget is spent. `instance` is the
/// agent class instance whose child it is, if it is one.
pub(crate) async fn run(
    registry: Registry,
    id: String,
    kind: ChildKind,
    agent: Agent,
    wake: Arc<Notify>,
    mut spent: watch::Receiver<bool>,
    instance: Option<u64>,
) {
    let mut baseline = Summary::default();
    // Why the child's model can answer nothing, once a round has found so.
    let mut broken: Option<String> = None;
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
            for request in children.open(&id) {
                children.settle(&id, request, outcome, &why);
            }
            children.idle(&id);
            drop(children);
            wake.notified().await;
            continue;
        }
        let live = children.summary(&id, kind);
        if orienting.is_none() && (live == baseline || live.is_empty()) {
            if kind == ChildKind::Work
                && let Some(&submission) = live.taken.first()
            {
                // The round that asked for it ended without it too. The
                // baseline stays, so the submission is not asked for again
                // before its end is heard; asking to end one that has ended
                // changes nothing.
                children.settle(
                    &id,
                    submission,
                    "rejected",
                    &format!(
                        "the child ended two rounds without completing submission {submission}, \
                         so it ended without completing"
                    ),
                );
            }
            children.idle(&id);
            drop(children);
            tokio::select! {
                () = wake.notified() => {}
                _ = spent.wait_for(|spent| *spent) => {}
            }
            continue;
        }
        // Opened before a submission is given, so its taking names this round.
        if !children.round_opened(&id, agent.history.next_round()) {
            return;
        }
        let (lead, detail) = match kind {
            _ if orienting.is_some() => (orientation::ORIENTING.to_string(), String::new()),
            ChildKind::Request => (
                orientation::requests(&children.unread_text(&id), &children.taken(&id)),
                String::new(),
            ),
            ChildKind::Work => match live.taken.first() {
                Some(&submission) => {
                    let result_type = children
                        .submission(submission)
                        .map(|given| given.result_type)
                        .unwrap_or_default();
                    (orientation::nudge(submission, &result_type), String::new())
                }
                // Given now, between rounds, and the round opens on whichever
                // it was: the one counted may have been cancelled meanwhile.
                None => {
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
                // An orienting round's baseline stays empty, so what is open as
                // it ends opens the next round.
                match orienting.take() {
                    Some(instance) => children.oriented(&id, instance, None),
                    None => baseline = live,
                }
                wait = RETRY_FIRST;
            }
            Err(AgentError::Closed(_)) => return,
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

/// Wait `wait`, and double it for the failure after, up to [`RETRY_MAX`].
async fn back_off(wait: &mut Duration) {
    tokio::time::sleep(*wait).await;
    *wait = (*wait * 2).min(RETRY_MAX);
}
