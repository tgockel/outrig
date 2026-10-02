//! What one model call may carry, and a conservative guess at what a message
//! costs.
//!
//! A model's window holds a request and its reply together. So a call's
//! **room** is the window, less a reserve for the reply, less what every call
//! carries whatever the conversation says: the system prompt and the tool
//! definition. What of the conversation a call is sent has to fit in that room,
//! and it has to be decided before the call, since a provider's report of what
//! a request cost arrives only after it has been accepted or refused
//! (`plan/phase/0003-python/history.md`).
//!
//! The window is the candidate's `context-window`, or [`ASSUMED_CONTEXT_WINDOW`]
//! with a warning when it names none. It is never inferred from the model's
//! identifier: a guess that looks like knowledge is the failure this avoids.
//!
//! There is no tokenizer here, and each provider counts differently, so tokens
//! are estimated from the serialized message: a token for every
//! [`BYTES_PER_TOKEN`] ASCII bytes, and one for every other character. That
//! over-counts code and prose, which is the safe direction; text that tokenizes
//! densely -- hex, base64 -- can still cost more than it is estimated at.

use std::io;

use rig::completion::Message;
use rig::completion::message::{AssistantContent, ReasoningContent};

use super::resolve::{LlmResolveError, ResolvedCandidate, ResolvedProvider};

/// The window a model is held to when its row names none. Below the window of
/// every current hosted model this loop is meant for, so a call is rarely
/// refused for its size; a model with a larger window is sent less than it
/// could take until its row says so.
pub(crate) const ASSUMED_CONTEXT_WINDOW: u32 = 128_000;

/// The reply's reserve when no output-token ceiling is sent -- an OpenAI-style
/// provider whose model sets no `max-tokens` -- so the provider's own default
/// governs, and nothing here knows it.
pub(crate) const DEFAULT_REPLY_RESERVE: u32 = 8_192;

/// The least room a call can have and still be worth starting a session for.
pub(crate) const MIN_ROOM: u64 = 1_024;

/// How many ASCII bytes are counted as one token. Code and English run nearer
/// four.
const BYTES_PER_TOKEN: u64 = 3;

/// What a provider adds to a request that offers tools, beyond the definitions
/// themselves: Anthropic documents a few hundred tokens of its own system
/// prompt for tool use.
const TOOL_USE_OVERHEAD: u64 = 512;

/// One candidate's allowance: what a model call to it may carry, the
/// output-token ceiling the call carries, and the protocol it is carried in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Budget {
    /// The candidate's `[models.<name>]` row.
    pub(crate) model: String,
    /// The whole window, in tokens.
    pub(crate) window: u32,
    /// Whether `window` is [`ASSUMED_CONTEXT_WINDOW`] rather than the row's own.
    pub(crate) window_assumed: bool,
    /// Tokens held back for the reply.
    pub(crate) reserve: u32,
    /// What every call carries besides the conversation: the system prompt and
    /// the tool definitions, estimated.
    pub(crate) overhead: u64,
    /// The output-token ceiling a call to this candidate carries on the wire:
    /// the configured one, filled in or lowered to what the model publishes, or
    /// `None` when none is sent.
    pub(crate) max_tokens: Option<u32>,
    /// The protocol the candidate's provider speaks, which settles what it can
    /// be sent of replies other providers wrote.
    pub(crate) wire: Wire,
}

/// Which provider's protocol a candidate speaks, for what a call to it can
/// carry of a conversation other providers wrote.
///
/// Reasoning is the part that does not travel. Anthropic's API takes a
/// thinking block back only with the signature it was signed with, and refuses
/// a request carrying one without. An OpenAI-compatible provider's reasoning
/// (`reasoning_content`) has no signature, so a call to Anthropic leaves it
/// out; Anthropic's own, signed or redacted, goes back whole. rig's OpenAI
/// adapter sends any reasoning as `reasoning_content` beside its reply, so an
/// OpenAI-compatible candidate takes everything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    OpenAi,
    Anthropic,
}

impl Wire {
    /// Whether a call in this protocol can carry `part` of a model's reply.
    pub(crate) fn takes(self, part: &AssistantContent) -> bool {
        match (self, part) {
            (Wire::Anthropic, AssistantContent::Reasoning(reasoning)) => {
                reasoning.content.iter().all(|block| {
                    matches!(
                        block,
                        ReasoningContent::Text {
                            signature: Some(_),
                            ..
                        } | ReasoningContent::Redacted { .. }
                    )
                })
            }
            _ => true,
        }
    }
}

impl Budget {
    /// The allowance for `candidate`, whose replies are held to `max_tokens` --
    /// the ceiling that reaches the wire, which is not always the one
    /// configured -- on calls that each carry `overhead` besides the
    /// conversation.
    ///
    /// The reserve is the reply's ceiling when there is one. Against an assumed
    /// window it is at most a quarter of it: that ceiling was chosen against
    /// the model's real window, and holding a guess to it could leave a call
    /// no room for a full-size tool result.
    pub(crate) fn new(
        candidate: &ResolvedCandidate,
        max_tokens: Option<u32>,
        overhead: u64,
    ) -> Result<Budget, LlmResolveError> {
        // Never from `candidate.model_identifier`: see the module header.
        let (window, window_assumed) = match candidate.context_window {
            Some(window) => (window, false),
            None => (ASSUMED_CONTEXT_WINDOW, true),
        };
        let quarter = window / 4;
        let reserve = match (max_tokens, window_assumed) {
            (Some(ceiling), false) => ceiling,
            (Some(ceiling), true) => ceiling.min(quarter),
            (None, _) => DEFAULT_REPLY_RESERVE.min(quarter),
        };
        let budget = Budget {
            model: candidate.model_name.clone(),
            window,
            window_assumed,
            reserve,
            overhead,
            max_tokens,
            wire: match candidate.provider {
                ResolvedProvider::OpenAi { .. } => Wire::OpenAi,
                ResolvedProvider::Anthropic { .. } => Wire::Anthropic,
            },
        };
        if budget.room() < MIN_ROOM {
            return Err(LlmResolveError::WindowTooSmall(budget));
        }
        if window_assumed {
            tracing::warn!(
                "[models.{model}] sets no context-window, so each model call is held to an \
                 assumed {window}-token window, {reserve} of it reserved for the reply. Set \
                 [models.{model}].context-window to the model's own window: a larger one lets \
                 each call carry more of the conversation.",
                model = budget.model,
            );
        }
        Ok(budget)
    }

    /// What of the conversation one call may carry.
    pub(crate) fn room(&self) -> u64 {
        u64::from(self.window)
            .saturating_sub(u64::from(self.reserve))
            .saturating_sub(self.overhead)
    }

    /// An allowance with `room` tokens for the conversation, for tests that
    /// size a view by the estimates of the turns in it.
    #[cfg(test)]
    pub(crate) fn with_room(model: &str, room: u64) -> Budget {
        Budget {
            model: model.to_string(),
            window: u32::try_from(room).expect("a test's room fits a window"),
            window_assumed: false,
            reserve: 0,
            overhead: 0,
            max_tokens: None,
            wire: Wire::OpenAi,
        }
    }
}

/// Why `budget` leaves too little room for a round, and what to change.
pub(crate) fn too_small(budget: &Budget) -> String {
    let Budget {
        model,
        window,
        window_assumed,
        reserve,
        overhead,
        ..
    } = budget;
    let assumed = if *window_assumed { " (assumed)" } else { "" };
    format!(
        "model {model:?}: a {window}-token context window{assumed} leaves about {} tokens for the \
         conversation after {reserve} reserved for the reply and about {overhead} for the system \
         prompt and tool definition, and a round needs at least {MIN_ROOM}. Set \
         [models.{model}].context-window to the model's whole window, or lower max-tokens",
        budget.room()
    )
}

/// A candidate on an OpenAI-style provider named `model`, with `context_window`,
/// for tests that build budgets from one.
#[cfg(test)]
pub(crate) fn candidate(model: &str, context_window: Option<u32>) -> ResolvedCandidate {
    use super::resolve::ResolvedProvider;
    ResolvedCandidate {
        model_name: model.into(),
        model_identifier: model.into(),
        provider_name: "p".into(),
        provider: ResolvedProvider::OpenAi {
            base_url: "http://127.0.0.1:1".into(),
            api_key: "k".into(),
            request_timeout_secs: None,
            retry_budget_secs: None,
        },
        max_tokens: None,
        context_window,
    }
}

/// What every call carries besides the conversation: `preamble`, and the tool
/// definitions described by `tools` (each a name, a description, and a
/// parameter schema, as text).
pub(crate) fn overhead<'a>(preamble: &str, tools: impl IntoIterator<Item = &'a str>) -> u64 {
    let tools: u64 = tools.into_iter().map(text_tokens).sum();
    text_tokens(preamble) + tools + TOOL_USE_OVERHEAD
}

/// The estimated tokens of `messages`, as the JSON they serialize to.
pub(crate) fn tokens<'a>(messages: impl IntoIterator<Item = &'a Message>) -> u64 {
    let mut count = Count::default();
    for message in messages {
        // Streamed rather than rendered: a tool result can be a quarter of a
        // megabyte, and only its length is wanted. Writing to a counter cannot
        // fail, and a message that will not serialize is counted as far as it
        // got.
        let _ = serde_json::to_writer(&mut count, message);
    }
    count.tokens()
}

/// The estimated tokens of `text`.
pub(crate) fn text_tokens(text: &str) -> u64 {
    let mut count = Count::default();
    count.add(text.as_bytes());
    count.tokens()
}

/// ASCII bytes and other characters, counted apart: a character outside ASCII
/// is a token or more on every tokenizer, where its two to four bytes would
/// count as one or less.
#[derive(Default)]
struct Count {
    ascii: u64,
    other: u64,
}

impl Count {
    fn add(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte.is_ascii() {
                self.ascii += 1;
            } else if byte >= 0xC0 {
                // A lead byte: one per character. Continuation bytes are
                // 0x80..0xC0 and are not counted.
                self.other += 1;
            }
        }
    }

    fn tokens(&self) -> u64 {
        self.ascii.div_ceil(BYTES_PER_TOKEN) + self.other
    }
}

impl io::Write for Count {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.add(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reserve_is_the_ceiling_on_the_wire_or_a_quarter_of_a_guess() {
        let configured = Budget::new(&candidate("m", Some(200_000)), Some(64_000), 100).unwrap();
        assert_eq!(
            (
                configured.window,
                configured.window_assumed,
                configured.reserve
            ),
            (200_000, false, 64_000)
        );
        assert_eq!(configured.room(), 200_000 - 64_000 - 100);

        let assumed = Budget::new(&candidate("m", None), Some(64_000), 100).unwrap();
        assert_eq!(
            (assumed.window, assumed.window_assumed, assumed.reserve),
            (ASSUMED_CONTEXT_WINDOW, true, ASSUMED_CONTEXT_WINDOW / 4)
        );
        let small_ceiling = Budget::new(&candidate("m", None), Some(4_096), 100).unwrap();
        assert_eq!(small_ceiling.reserve, 4_096);

        let unsent = Budget::new(&candidate("m", Some(16_000)), None, 100).unwrap();
        assert_eq!(unsent.reserve, 4_000, "a quarter of a small window");
        let unsent = Budget::new(&candidate("m", None), None, 100).unwrap();
        assert_eq!(unsent.reserve, DEFAULT_REPLY_RESERVE);
    }

    #[test]
    fn a_window_with_no_room_left_is_refused() {
        let err = Budget::new(&candidate("m", Some(10_000)), Some(8_000), 1_500).unwrap_err();
        let text = err.to_string();
        assert!(matches!(&err, LlmResolveError::WindowTooSmall(budget) if budget.room() == 500));
        assert!(text.contains("[models.m].context-window"), "{text}");
        assert!(text.contains("8000 reserved for the reply"), "{text}");
    }

    #[test]
    fn a_turns_estimate_is_conservative() {
        // Plain text: a token per three bytes, rounded up.
        assert_eq!(text_tokens("abcdefg"), 3);
        // A character outside ASCII is a token of its own, not a third of its
        // three bytes.
        assert_eq!(text_tokens("日本語"), 3);
        // A message costs its JSON, so more than its text alone.
        let text = "x".repeat(3_000);
        let message = Message::user(text.as_str());
        assert!(tokens([&message]) > text_tokens(&text));
        assert!(tokens([&message]) < text_tokens(&text) + 50);
    }
}
