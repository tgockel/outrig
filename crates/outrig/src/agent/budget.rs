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
//! rule is a starting point, not a bound. It over-counts code and prose on some
//! tokenizers and under-counts them on others -- a request to Claude Opus 5 was
//! reported at about 11% more than the rule estimated it -- and text that
//! tokenizes densely, such as hex or base64, costs more again.
//!
//! So a candidate's estimate is scaled by what its own provider has reported
//! ([`Scale`]): the ratio of the input tokens its latest answered call was
//! reported at to what the rule estimated for that call, and never less than
//! the rule. A candidate that has not answered is held to the rule alone, and
//! one candidate's ratio says nothing of another's, since their tokenizers can
//! differ. A scale never refuses a prompt the rule admits: the reply's reserve
//! gives way first ([`Budget::making_room_for`]).

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use rig::completion::message::{AssistantContent, ReasoningContent};
use rig::completion::{Message, Usage};

use super::resolve::{LlmResolveError, ResolvedCandidate, ResolvedProvider};
use crate::config::RoleAlternation;

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

/// The least a reply's ceiling is lowered to, by [`Budget::making_room_for`]: a
/// ceiling of nothing gets an answer of nothing, which a retry repeats.
pub(crate) const MIN_REPLY: u32 = 1_024;

/// How many ASCII bytes are counted as one token. Code and English run nearer
/// four.
const BYTES_PER_TOKEN: u64 = 3;

/// What a provider adds to a request that offers tools, beyond the definitions
/// themselves: Anthropic documents a few hundred tokens of its own system
/// prompt for tool use.
const TOOL_USE_OVERHEAD: u64 = 512;

/// What the rule's estimate of a request is multiplied by to be held against
/// the window, in thousandths: how far a provider's own count of its requests
/// has run over the rule's. 1.0 is the rule as it stands.
///
/// Never below the rule, and never above [`Scale::MAX`]: the rule counts a
/// token per [`BYTES_PER_TOKEN`] ASCII bytes and no tokenizer counts more than a
/// token per byte, so a larger ratio is a report that means something else.
/// Believing one could leave a call no room for its own prompt, and with no call
/// made there would be no report to correct it. [`Budget::making_room_for`]
/// guards a call to a window a smaller scale can still fill the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Scale(u64);

/// The unit of [`Scale`].
const THOUSANDTHS: u64 = 1_000;

impl Scale {
    /// The rule as it stands.
    pub(crate) const RULE: Scale = Scale(THOUSANDTHS);

    const MAX: Scale = Scale(BYTES_PER_TOKEN * THOUSANDTHS);

    /// The scale a request reported at `reported` input tokens, where the rule
    /// estimated `estimate`, calls for.
    pub(crate) fn observed(reported: u64, estimate: u64) -> Scale {
        let thousandths = reported.saturating_mul(THOUSANDTHS).div_ceil(estimate);
        Scale(thousandths.clamp(Self::RULE.0, Self::MAX.0))
    }

    /// The rule's tokens that fit in `provider` tokens of the provider's own.
    fn fits_in(self, provider: u64) -> u64 {
        provider.saturating_mul(THOUSANDTHS) / self.0
    }

    /// The provider's tokens that `rule` tokens of the rule's come to.
    fn of(self, rule: u64) -> u64 {
        rule.saturating_mul(self.0).div_ceil(THOUSANDTHS)
    }

    /// The largest scale at which `rule` tokens of the rule's still fit in
    /// `window` tokens of the provider's own, though not below the rule.
    fn fitting(window: u64, rule: u64) -> Scale {
        Scale((window.saturating_mul(THOUSANDTHS) / rule).max(Self::RULE.0))
    }

    /// As a multiple of the rule's estimate.
    pub(crate) fn ratio(self) -> f64 {
        self.0 as f64 / THOUSANDTHS as f64
    }
}

/// What one candidate's provider has been seen to count: the [`Scale`] of its
/// latest answered call.
#[derive(Debug)]
pub(crate) struct Calibration(AtomicU64);

impl Default for Calibration {
    fn default() -> Self {
        Self(AtomicU64::new(Scale::RULE.0))
    }
}

impl Calibration {
    /// The scale the candidate's next call is held to.
    pub(crate) fn get(&self) -> Scale {
        Scale(self.0.load(Ordering::Relaxed))
    }

    /// An answered call whose request the provider reported at `reported`
    /// input tokens, and the rule estimated at `estimate`. The latest call
    /// replaces the one before: consecutive views are nearly the same text, so
    /// it is the best guide to the next, and a dense turn that leaves the view
    /// stops costing room. A call that reported nothing, or estimated nothing,
    /// says nothing.
    pub(crate) fn learn(&self, reported: u64, estimate: u64) {
        if reported == 0 || estimate == 0 {
            return;
        }
        self.0
            .store(Scale::observed(reported, estimate).0, Ordering::Relaxed);
    }
}

/// One candidate's allowance: what a model call to it may carry, the
/// output-token ceiling the call carries, and the protocol it is carried in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Budget {
    /// The candidate's `[models.<name>]` row.
    pub(crate) model: String,
    /// The `[providers.<name>]` row that model row names.
    pub(crate) provider: String,
    /// The whole window, in tokens.
    pub(crate) window: u32,
    /// Whether `window` is [`ASSUMED_CONTEXT_WINDOW`] rather than the row's own.
    pub(crate) window_assumed: bool,
    /// Tokens held back for the reply.
    pub(crate) reserve: u32,
    /// What every call carries besides the conversation: the system prompt and
    /// the tool definitions, estimated.
    pub(crate) overhead: u64,
    /// What the estimate of a call's whole request is multiplied by to be held
    /// against the window.
    pub(crate) scale: Scale,
    /// The output-token ceiling a call to this candidate carries on the wire:
    /// the configured one, filled in or lowered to what the model publishes, or
    /// `None` when none is sent.
    pub(crate) max_tokens: Option<u32>,
    /// The protocol the candidate's provider speaks, which settles what it can
    /// be sent of replies other providers wrote.
    pub(crate) wire: Wire,
    /// Whether the provider requires the user's and the model's turns to
    /// alternate, which settles what a call to it can be sent of a
    /// conversation cut between turns.
    pub(crate) alternation: RoleAlternation,
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
    /// Whether a call in this protocol can carry `part` of a model's reply,
    /// in a turn whose thinking is `stale`: bound to a conversation a later
    /// call changed what came before it in, which Anthropic refuses.
    pub(crate) fn takes(self, part: &AssistantContent, stale: bool) -> bool {
        match (self, part) {
            (Wire::Anthropic, AssistantContent::Reasoning(reasoning)) => {
                !stale
                    && reasoning.content.iter().all(|block| {
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

    /// The input tokens a call in this protocol was reported to have cost, from
    /// `usage`. Anthropic's `input_tokens` leaves out what was read from and
    /// written to its cache, which the request carried all the same; an
    /// OpenAI-compatible provider's counts them.
    pub(crate) fn reported_input(self, usage: &Usage) -> u64 {
        match self {
            Wire::OpenAi => usage.input_tokens,
            Wire::Anthropic => usage
                .input_tokens
                .saturating_add(usage.cached_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
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
            provider: candidate.provider_name.clone(),
            window,
            window_assumed,
            reserve,
            overhead,
            scale: Scale::RULE,
            max_tokens,
            wire: match candidate.provider {
                ResolvedProvider::OpenAi { .. } => Wire::OpenAi,
                ResolvedProvider::Anthropic { .. } => Wire::Anthropic,
            },
            alternation: candidate.provider.role_alternation(),
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

    /// What of the conversation one call may carry, in the rule's tokens: the
    /// whole request, scaled, has to fit in the window less the reply's reserve.
    pub(crate) fn room(&self) -> u64 {
        self.room_at(self.scale)
    }

    fn room_at(&self, scale: Scale) -> u64 {
        let request = u64::from(self.window).saturating_sub(u64::from(self.reserve));
        scale.fits_in(request).saturating_sub(self.overhead)
    }

    /// This allowance for a call whose estimate is multiplied by `scale`.
    pub(crate) fn scaled(&self, scale: Scale) -> Budget {
        Budget {
            scale,
            ..self.clone()
        }
    }

    /// This allowance for a call whose prompt, which the call cannot be sent
    /// without, is `prompt` tokens of the rule's: as it stands, unless its scale
    /// would refuse a prompt the rule admits.
    ///
    /// A scale can take all the room a call has: a system prompt its provider
    /// counts at half again the rule's estimate may leave the window less than
    /// the reserve beside it. A call that is not made reports nothing to correct
    /// the scale with, so the reply gives way. The window holds the request as
    /// the provider counts it and the reply together, so the reserve, and the
    /// ceiling sent with it, are lowered to what the request leaves, though not
    /// below [`MIN_REPLY`]. If not even that leaves the room, the scale is cut
    /// back to the one that does: it was learned from some other request, and
    /// this one is the only way to learn it again. What the rule refuses is
    /// still refused.
    pub(crate) fn making_room_for(&self, prompt: u64) -> Budget {
        if self.room() >= prompt || self.room_at(Scale::RULE) < prompt {
            return self.clone();
        }
        let window = u64::from(self.window);
        let reply = self.reserve.min(MIN_REPLY);
        let carried = self.overhead.saturating_add(prompt);
        let scale = self.scale.min(Scale::fitting(
            window.saturating_sub(u64::from(reply)),
            carried,
        ));
        let reserve = u32::try_from(window.saturating_sub(scale.of(carried)))
            .map_or(self.reserve, |left| self.reserve.min(left));
        Budget {
            scale,
            reserve,
            // A ceiling above the reserve, because the window is assumed to be
            // smaller than the model's own, goes down only with the reserve.
            max_tokens: if reserve < self.reserve {
                self.max_tokens.map(|ceiling| ceiling.min(reserve))
            } else {
                self.max_tokens
            },
            ..self.clone()
        }
    }

    /// An allowance with `room` tokens for the conversation, for tests that
    /// size a view by the estimates of the turns in it.
    #[cfg(test)]
    pub(crate) fn with_room(model: &str, room: u64) -> Budget {
        Budget {
            model: model.to_string(),
            provider: "p".to_string(),
            window: u32::try_from(room).expect("a test's room fits a window"),
            window_assumed: false,
            reserve: 0,
            overhead: 0,
            scale: Scale::RULE,
            max_tokens: None,
            wire: Wire::OpenAi,
            alternation: RoleAlternation::Relaxed,
        }
    }

    /// This allowance for a provider that requires the user's and the model's
    /// turns to alternate.
    #[cfg(test)]
    pub(crate) fn strict(self) -> Budget {
        Budget {
            alternation: RoleAlternation::Strict,
            ..self
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
            role_alternation: RoleAlternation::Relaxed,
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
    fn the_scale_is_what_was_reported_over_what_was_estimated_within_bounds() {
        // The issue's call 76: 95,863 estimated, 107,869 reported. Rounded up.
        let observed = Scale::observed(107_869, 95_863);
        assert_eq!(observed, Scale(1_126));
        assert_eq!(observed.ratio(), 1.126);
        // Never below the rule: the first call there over-counted.
        assert_eq!(Scale::observed(1_454, 1_724), Scale::RULE);
        // Never above what no tokenizer could count.
        assert_eq!(Scale::observed(900_000, 1_000), Scale::MAX);
    }

    #[test]
    fn the_room_is_what_fits_once_the_estimate_is_scaled() {
        let rule = Budget::new(&candidate("m", Some(200_000)), Some(64_000), 1_000).unwrap();
        assert_eq!(rule.room(), 136_000 - 1_000);

        // The whole request is scaled, the overhead with it.
        let scaled = rule.scaled(Scale(1_250));
        assert_eq!(scaled.scale, Scale(1_250));
        assert_eq!(scaled.room(), 136_000 * 4 / 5 - 1_000);
        assert_eq!(rule.scaled(Scale::RULE), rule);
    }

    /// A system prompt the provider counts at half again the rule's estimate
    /// leaves the window less than the reserve beside it. A prompt the scale
    /// would refuse is carried all the same, by lowering the reply: the reserve
    /// and the ceiling that goes out are what the request leaves.
    #[test]
    fn a_prompt_the_scale_would_refuse_takes_the_room_from_the_reply() {
        let rule = Budget::new(&candidate("m", Some(200_000)), Some(64_000), 100_000).unwrap();
        assert_eq!(rule.room(), 36_000);
        let scaled = rule.scaled(Scale(1_500));
        assert_eq!(scaled.room(), 0);

        // 102_000 of the rule's tokens are 153_000 of the provider's.
        let made = scaled.making_room_for(2_000);
        assert_eq!(made.scale, Scale(1_500), "the report is believed");
        assert_eq!(made.reserve, 200_000 - 153_000);
        assert_eq!(made.max_tokens, Some(made.reserve));
        assert_eq!(made.room(), 2_000);
        assert_eq!(made.window, rule.window);

        // A prompt that fits is held to the allowance as it stands.
        assert_eq!(scaled.making_room_for(0), scaled);
        let roomy = Budget::new(&candidate("m", Some(200_000)), Some(64_000), 1_000).unwrap();
        let roomy = roomy.scaled(Scale(1_250));
        assert_eq!(roomy.making_room_for(100_000), roomy);
        // And one the rule itself refuses is refused.
        assert_eq!(scaled.making_room_for(36_001), scaled);
    }

    /// A ceiling of nothing gets an answer of nothing, so the reply is lowered
    /// no further than [`MIN_REPLY`], and the scale is cut back instead.
    #[test]
    fn the_reply_is_never_lowered_to_nothing() {
        let rule = Budget::new(&candidate("m", Some(200_000)), Some(64_000), 98_976).unwrap();
        let scaled = rule.scaled(Scale(2_000));

        let made = scaled.making_room_for(1_000);
        assert!(made.reserve >= MIN_REPLY, "reserve {}", made.reserve);
        assert_eq!(made.max_tokens, Some(made.reserve));
        assert!(Scale::RULE < made.scale && made.scale < scaled.scale);
        assert!(made.room() >= 1_000, "room {}", made.room());
    }

    /// An assumed window is smaller than the model's own, so the ceiling it is
    /// sent is above the reserve; that goes down with the reserve and not
    /// before.
    #[test]
    fn a_ceiling_above_an_assumed_reserve_is_kept_while_the_reserve_fits() {
        let rule = Budget::new(&candidate("m", None), Some(64_000), 1_000).unwrap();
        assert_eq!((rule.reserve, rule.max_tokens), (32_000, Some(64_000)));

        let scaled = rule.scaled(Scale(1_250));
        assert_eq!(scaled.making_room_for(2_000), scaled);
    }

    /// A model whose row sets no ceiling is sent none, however far the reserve
    /// held back for its default is lowered.
    #[test]
    fn a_lowered_reserve_leaves_a_missing_ceiling_missing() {
        let rule = Budget::new(&candidate("m", Some(40_000)), None, 25_000).unwrap();
        assert_eq!(
            (rule.reserve, rule.max_tokens),
            (DEFAULT_REPLY_RESERVE, None)
        );

        let made = rule.scaled(Scale(1_500)).making_room_for(2_000);
        assert!(made.reserve < rule.reserve);
        assert_eq!(made.max_tokens, None);
        assert!(made.room() >= 2_000, "room {}", made.room());
    }

    #[test]
    fn a_provider_reports_its_input_as_its_protocol_counts_it() {
        let usage = Usage {
            input_tokens: 100,
            cached_input_tokens: 50,
            cache_creation_input_tokens: 5,
            ..Usage::new()
        };
        // OpenAI's prompt tokens already include what was cached.
        assert_eq!(Wire::OpenAi.reported_input(&usage), 100);
        // Anthropic's leave it out.
        assert_eq!(Wire::Anthropic.reported_input(&usage), 155);
    }

    #[test]
    fn a_calibration_follows_the_latest_call_and_ignores_one_that_said_nothing() {
        let calibration = Calibration::default();
        assert_eq!(calibration.get(), Scale::RULE);

        calibration.learn(1_200, 1_000);
        assert_eq!(calibration.get(), Scale(1_200));
        calibration.learn(1_050, 1_000);
        assert_eq!(calibration.get(), Scale(1_050));
        // The latest, so one that ran under the rule puts it back.
        calibration.learn(900, 1_000);
        assert_eq!(calibration.get(), Scale::RULE);

        calibration.learn(1_200, 1_000);
        calibration.learn(0, 1_000);
        calibration.learn(1_000, 0);
        assert_eq!(calibration.get(), Scale(1_200));
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
