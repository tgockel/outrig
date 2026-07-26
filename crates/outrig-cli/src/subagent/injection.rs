//! Delivering a parent's prompt into a round already in flight.
//!
//! `outrig__subagent_send` to a *running* subagent cannot start a new round --
//! one is already going. Instead the prompt is queued and folded into the
//! history of the subagent's next model call, so it reads as something the
//! parent just said and the subagent can course-correct without finishing
//! first.
//!
//! Rig supports this directly: a hook on `StepEvent::CompletionCall` returns
//! `Flow::PatchRequest` with a replacement history. Two properties of that
//! patch drive the design here, both because it is **per-turn and
//! non-sticky** -- rig's docs are explicit that "the persisted transcript and
//! the run state are untouched":
//!
//! - Queued steers are *read*, not drained, on every model call. Dropping one
//!   after a single turn would make it vanish from the next call in the same
//!   round.
//! - The round's driver folds them into the subagent's own `Vec<Message>` when
//!   the round ends, since rig never persisted them and the following round
//!   would otherwise not remember being steered.

use rig::completion::Message;

/// Marks the text as coming from the parent rather than from the subagent's
/// own task, so a steer is not mistaken for part of the original assignment.
pub fn steer_message(text: &str) -> Message {
    Message::user(format!(
        "[message from the agent that launched you]\n{text}"
    ))
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
