//! Delivering a parent's prompt into a round already in flight.
//!
//! `outrig__subagent_send` to a *running* subagent cannot start a new round --
//! one is already going. Instead the prompt is queued as a steer and appended
//! to the subagent's next tool results, so its model reads it as something the
//! parent said while those calls ran, and the subagent can course-correct
//! without finishing first.
//!
//! A tool result is the one place it can go. A model call that follows a tool
//! call is prompted with that call's results, and rig always sends the prompt
//! last: a hook can patch only the history sent ahead of it, and a steer put
//! there separates the call from its results, which OpenAI and Anthropic
//! refuse (#230). So the hook rewrites a result to end with the steer, under
//! [`STEER_HEADER`], and rig keeps the result as rewritten -- see
//! [`crate::llm::InjectionSource`]. Three properties follow:
//!
//! - A steer goes out once. The result it rides on stays in the round's
//!   history, so every later model call, and every later round, sees it where
//!   the model first did. Nothing re-applies it, and the round's driver has
//!   nothing to fold in. A turn whose history ends up without that result --
//!   one that ends in an error keeps none of itself -- takes the delivery
//!   back, the way it takes back a parent's read (#251), and the steer counts
//!   as never sent.
//! - It rides on a batch's last result, after everything it interrupted, and
//!   only when a model call is going to read the batch. The tool-call cap and
//!   the repeat breaker end a round at its next model call without making it,
//!   and a steer written into a result no model call reads would sit in the
//!   history, acted on by nothing.
//! - A round's first model call follows no tool result. A steer accepted
//!   before it waits for the round's first batch, as one accepted during it
//!   does.
//!
//! Which model call is a round's last is unknown until the agent loop returns,
//! so the round stops taking steers at that moment, under the same lock
//! [`SubagentShared::accept`] decides under. From then on a prompt queues as a
//! round of its own, behind any already waiting. A steer no result carried
//! reached no model:
//!
//! - Unless the round failed, the steer joins that queue as a round of its own.
//!   Nothing was queued while the round took steers, so this puts it behind
//!   every prompt sent before it and ahead of every one sent after, in the
//!   order the parent sent them. The subagent does not go idle while a round
//!   is waiting.
//! - A failed round starts nothing on its own: its parent reads the failure, or
//!   the round's report if it sent nothing after that, and decides what comes
//!   next. The steer is folded into the history, to reach the model with that.
//!
//! Either way, no steer outlives its round.
//!
//! [`SubagentShared::accept`]: super::state::SubagentShared::accept

use rig::completion::Message;

/// The line a steer opens with; see [`steer_text`].
pub(crate) const STEER_HEADER: &str = "[message from the agent that launched you]";

/// Marks the text as coming from the parent rather than from the subagent's
/// own task or the tool result it rides on, so a steer is not mistaken for
/// either.
pub fn steer_text(text: &str) -> String {
    format!("{STEER_HEADER}\n{text}")
}

/// A steer as a message of its own, which a failed round folds into the
/// history when no tool result there carries it.
pub fn steer_message(text: &str) -> Message {
    Message::user(steer_text(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steer_is_attributed_to_the_parent() {
        let Message::User { content } = steer_message("stop") else {
            panic!("steer should be a user message");
        };
        let rendered = format!("{content:?}");
        assert!(
            rendered.contains("launched you"),
            "steer should say where it came from, got: {rendered}"
        );
    }
}
