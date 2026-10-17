//! `run-new`'s escalation handler: a binding call the session's `[policy]` holds for a person is
//! shown on stderr with its id, and answered at the prompt with `/approve <id>` or `/deny <id>`.
//!
//! The handler runs on the session's runtime and owns nothing of the terminal: it hands each
//! call to the conversation's loop, which prints it and keeps the answer's sender until the user
//! answers, and tells the loop when a call stops waiting -- its caller interrupted, the session
//! closing -- so the loop can say so and forget it.

use std::fmt::Write as _;

use outrig::harness::event::RequestId;
use outrig::harness::{
    Escalation, EscalationAnswer, EscalationHandler, EscalationResult, Withdrawn,
};
use tokio::sync::{mpsc, oneshot};

/// What the handler tells the conversation's loop.
pub(super) enum Asking {
    /// A call waiting for an answer, and where the answer goes.
    Pending(Box<Escalation>, oneshot::Sender<EscalationAnswer>),
    /// A call that stopped waiting before it was answered.
    Withdrawn(RequestId, Withdrawn),
}

/// The handler `run-new` installs.
pub(super) struct TerminalEscalation {
    asking: mpsc::UnboundedSender<Asking>,
}

impl TerminalEscalation {
    pub(super) fn new(asking: mpsc::UnboundedSender<Asking>) -> Self {
        Self { asking }
    }
}

impl EscalationHandler for TerminalEscalation {
    async fn escalate(&self, request: Escalation) -> EscalationResult {
        let id = request.request;
        let (answer, answered) = oneshot::channel();
        self.asking
            .send(Asking::Pending(Box::new(request.clone()), answer))
            .map_err(|_| "the terminal is gone, so nobody can answer")?;
        // An answer given as the call stopped waiting goes back, so the session records it as
        // ignored and the terminal says the answer changed nothing.
        tokio::select! {
            biased;
            answer = answered => Ok(answer.map_err(|_| "nobody is left to answer")?),
            why = request.withdrawn() => {
                let _ = self.asking.send(Asking::Withdrawn(id, why));
                Err("it stopped waiting for an answer".into())
            }
        }
    }
}

/// The two lines that show a waiting call: what it is, and how to answer it.
pub(super) fn pending_text(request: &Escalation) -> String {
    let id = request.request;
    // As the event log names it.
    let mut call = serde_json::to_value(request.operation)
        .ok()
        .and_then(|name| name.as_str().map(str::to_string))
        .unwrap_or_default();
    if let Some(member) = &request.member {
        let _ = write!(call, " {}", printable(member));
    }
    if let Some(target) = &request.target {
        let _ = write!(call, " on {}", printable(target));
    }
    let arguments: Vec<String> = request
        .arguments
        .iter()
        .map(|argument| printable(argument))
        .chain(
            request
                .keywords
                .iter()
                .map(|(name, value)| format!("{}={}", printable(name), printable(value))),
        )
        .collect();
    if !arguments.is_empty() {
        let _ = write!(call, " ({})", arguments.join(", "));
    }
    format!(
        "[outrig] binding call {id} waits for your answer: {}: {call}\n\
         [outrig]   {}; type /approve {id} or /deny {id}",
        printable(&request.binding),
        printable(&request.reason),
    )
}

/// `text` with every control or direction-changing character escaped: what a binding call
/// names comes from a library and from agent code, and a terminal would act on them.
fn printable(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        let direction = matches!(
            c,
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        );
        if c.is_control() || direction {
            shown.extend(c.escape_unicode());
        } else {
            shown.push(c);
        }
    }
    shown
}

/// What a withdrawn call's note says of why.
pub(super) fn withdrawn_text(id: RequestId, why: Withdrawn) -> String {
    let why = match why {
        Withdrawn::Interrupted => "its caller was interrupted",
        Withdrawn::Closed => "the session is closing",
        Withdrawn::Disconnected => "its connection closed",
        Withdrawn::Exited => "the bindings process ended",
        _ => "it stopped waiting",
    };
    format!("[outrig] binding call {id} no longer waits for an answer: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_and_direction_characters_are_escaped() {
        assert_eq!(printable("main"), "main");
        assert_eq!(printable("a\u{1b}[2Jb"), "a\\u{1b}[2Jb");
        assert_eq!(printable("x\u{202e}y\n"), "x\\u{202e}y\\u{a}");
    }
}
