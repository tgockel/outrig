//! The fixed question contract and an in-memory owner backend.
use super::Positions;
use super::channels::*;
use serde::{Deserialize, Serialize};
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Reserved outbound channel used by `runtime.ask`.
pub const QUESTION_CHANNEL: &str = "__questions";
/// Maximum open questions per asker, independently of `requests-max`.
pub const QUESTIONS_MAX: usize = 32;
/// Maximum number of choices in a question.
pub const QUESTION_CHOICES_MAX: usize = 32;
/// Maximum encoded question size, in bytes.
pub const QUESTION_BYTES_MAX: usize = 65536;

/// One offered choice, identified by its label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    /// The label returned in an answer.
    pub label: String,
    /// Optional explanatory text.
    #[serde(default)]
    pub description: String,
}
/// An inert file reference supplied as background, never opened by OutRig.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionReference {
    /// File path as the asker sees it.
    pub path: String,
    /// One-based starting line, if any.
    pub line: Option<u64>,
    /// Inclusive ending line; requires a start.
    pub end_line: Option<u64>,
    /// Why the file matters, as plain text.
    #[serde(default)]
    pub description: String,
}
/// Longer context for a question: background prose, file references, or both.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionContext {
    /// Background as untrusted plain text.
    #[serde(default)]
    pub description: String,
    /// Related files. OutRig does not read their contents.
    #[serde(default)]
    pub references: Vec<QuestionReference>,
}
/// A free-form, single-choice, or multi-select question.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// The question itself.
    pub text: String,
    /// Choices in display order; empty means free-form.
    pub choices: Vec<Choice>,
    /// Whether more than one choice may be selected.
    pub multiple: bool,
    /// Whether text is accepted alongside offered choices.
    pub other: bool,
    /// Optional longer background and file references, within the question-size bound.
    #[serde(default)]
    pub context: Option<QuestionContext>,
}
/// An explicit answer, with choices in the question's order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    /// Selected choice labels.
    pub choices: Vec<String>,
    /// Additional text, or the free-form answer.
    pub text: Option<String>,
    /// Who answered, for example `user` or `application`.
    pub by: String,
    /// Attribution path, reserved for escalation.
    pub chain: Vec<String>,
}
impl Question {
    pub(crate) fn validate(&self, bytes: usize) -> Result<(), String> {
        if bytes > QUESTION_BYTES_MAX {
            return Err(format!(
                "question exceeds size limit {QUESTION_BYTES_MAX} bytes"
            ));
        }
        if self.text.trim().is_empty() {
            return Err("question text must not be empty".into());
        }
        if self.choices.len() > QUESTION_CHOICES_MAX {
            return Err(format!(
                "question exceeds choices limit {QUESTION_CHOICES_MAX}"
            ));
        }
        let mut labels = std::collections::HashSet::new();
        for choice in &self.choices {
            if choice.label.trim().is_empty() || !labels.insert(&choice.label) {
                return Err("choice labels must be nonempty and unique".into());
            }
        }
        if let Some(context) = &self.context {
            for reference in &context.references {
                if reference.path.trim().is_empty() {
                    return Err("a question reference needs a nonempty path".into());
                }
                if reference.line == Some(0)
                    || reference
                        .end_line
                        .is_some_and(|end| reference.line.is_none_or(|start| end < start))
                {
                    return Err("question reference lines must be one-based and an end must follow its start".into());
                }
            }
        }
        Ok(())
    }
    pub(crate) fn answer(&self, value: &serde_json::Value) -> Result<serde_json::Value, String> {
        if value.to_string().len() > crate::python::host::MESSAGE_MAX {
            return Err("answer exceeds channel message size limit".into());
        }
        let mut answer: Answer =
            serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
        let mut labels = std::collections::HashSet::new();
        for label in &answer.choices {
            if !self.choices.iter().any(|c| &c.label == label) || !labels.insert(label.clone()) {
                return Err(format!("unknown or duplicate choice {label:?}"));
            }
        }
        if !self.multiple && answer.choices.len() > 1 {
            return Err("a single-choice question takes at most one choice".into());
        }
        if !self.choices.is_empty() && !self.other && answer.text.is_some() {
            return Err("this question does not accept free text (other=False)".into());
        }
        if (!self.multiple || self.choices.is_empty())
            && answer.choices.is_empty()
            && answer.text.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            return Err("an answer needs a choice or text".into());
        }
        answer.choices = self
            .choices
            .iter()
            .filter(|c| labels.contains(&c.label))
            .map(|c| c.label.clone())
            .collect();
        serde_json::to_value(answer).map_err(|e| e.to_string())
    }
}

/// Owner notifications. A withdrawal removes a question from a user interface.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum QuestionNotice {
    /// A new question, with request id, asker, and JSON body.
    Asked(Outbound),
    /// A question was withdrawn or the session closed.
    Withdrawn(Withdrawal),
    /// The runtime ended a question without an owner answer.
    Settled(Settled),
}
/// An in-memory backend for applications that handle questions on their own event loop.
/// Settle requests through `session.channel(QUESTION_CHANNEL).unwrap().settle(...)`.
/// Receipts and withdrawals do not wait for a person to answer.
pub struct QuestionBackend {
    notices: mpsc::UnboundedSender<QuestionNotice>,
}
impl QuestionBackend {
    /// Make a backend and its notification stream. Give the backend to
    /// `SessionBuilder::question_backend` before starting the session.
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<QuestionNotice>) {
        let (notices, receiver) = mpsc::unbounded_channel();
        (Arc::new(Self { notices }), receiver)
    }
}
impl ChannelBackend for QuestionBackend {
    fn send(&self, item: Outbound) -> BackendFuture<'_, SendAnswer> {
        let sent = self.notices.send(QuestionNotice::Asked(item));
        Box::pin(async move {
            if sent.is_ok() {
                SendAnswer::Accepted { reference: None }
            } else {
                SendAnswer::NotAccepted {
                    reason: "this application does not take questions".into(),
                }
            }
        })
    }
    fn consumed(&self, _: Consumed) -> BackendFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
    fn settled(&self, notice: Settled) -> BackendFuture<'_, io::Result<()>> {
        let _ = self.notices.send(QuestionNotice::Settled(notice));
        Box::pin(async { Ok(()) })
    }
    fn withdrawn(&self, notice: Withdrawal) -> BackendFuture<'_, io::Result<WithdrawAnswer>> {
        let _ = self.notices.send(QuestionNotice::Withdrawn(notice));
        Box::pin(async { Ok(WithdrawAnswer::Withdrawn) })
    }
    fn resolve_send(&self, _: Query) -> BackendFuture<'_, SendAnswer> {
        Box::pin(async { SendAnswer::Accepted { reference: None } })
    }
    fn resolve_withdrawal(&self, _: Query) -> BackendFuture<'_, io::Result<WithdrawAnswer>> {
        Box::pin(async { Ok(WithdrawAnswer::Withdrawn) })
    }
    fn positions(&self, _: Positions) -> BackendFuture<'_, io::Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

pub(crate) fn definition() -> ChannelDefinition {
    ChannelDefinition::new(QUESTION_CHANNEL)
        .kind(ChannelKind::Request(Direction::Outbound))
        .sends(Contract::Typed("outrig:Question".into()))
        .receives(Contract::Typed("outrig:Answer".into()))
        .bound(QUESTIONS_MAX)
        .description(
            "Questions to the session owner; prefer runtime.ask(question). No answer timeout.",
        )
}
