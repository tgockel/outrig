//! Everything the screen was told, in order: the conversation and the agent's
//! working beside it. The detail view draws it whole; the conversation shows
//! what is not detail as it arrives.
//!
//! It holds at most [`BOUND`] bytes of text, dropping the oldest entries past
//! that and counting them. `[events] mode = "record"` keeps a session's
//! everything; this keeps what a person scrolls back through.

use std::collections::VecDeque;
use std::fmt::Write as _;

use ratatui::text::Line;
use tracing::Level;

use super::facts::Fact;
use super::render::{Theme, entry_lines};
use super::status::elapsed;
use crate::cli::log_divert::Record;

const BOUND: usize = 4 << 20;

/// What an entry is, which decides where it shows and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A message the user sent.
    User,
    /// A message the agent sent the user.
    Agent,
    /// What the session says about itself in the conversation.
    Note,
    /// An error, in the conversation.
    Error,
    /// A word about the screen itself.
    Hint,
    /// The agent's working: the detail view's, and the conversation's too
    /// with `-v`.
    Detail,
    /// A log record below a warning: the detail view's only, so `-vv`'s
    /// trace does not fill the conversation.
    Trace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) kind: Kind,
    pub(super) title: Option<String>,
    pub(super) text: String,
}

impl Entry {
    /// An entry of `text`, less the newlines around it: the line mode frames
    /// what it writes with them, and here each entry is its own rows.
    pub(super) fn new(kind: Kind, text: impl Into<String>) -> Self {
        let mut text = text.into();
        text.truncate(text.trim_end_matches('\n').len());
        let start = text.len() - text.trim_start_matches('\n').len();
        text.drain(..start);
        Self {
            kind,
            title: None,
            text,
        }
    }

    pub(super) fn titled(kind: Kind, title: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::new(kind, text)
        }
    }

    /// Whether the conversation shows it, and not only the detail view.
    pub(super) fn inline(&self, verbose: bool) -> bool {
        match self.kind {
            Kind::User | Kind::Agent | Kind::Note | Kind::Error | Kind::Hint => true,
            Kind::Detail => verbose,
            Kind::Trace => false,
        }
    }

    /// What the conversation shows of a log record: an error in it, a
    /// warning as detail, and anything quieter in the detail view alone.
    pub(super) fn from_record(record: Record) -> Self {
        let kind = match record.level {
            Level::ERROR => Kind::Error,
            Level::WARN => Kind::Detail,
            _ => Kind::Trace,
        };
        Self::new(kind, record.text)
    }

    /// What the transcript records of `fact`, if anything.
    pub(super) fn from_fact(fact: &Fact) -> Option<Self> {
        let entry = match fact {
            Fact::RoundStarted { round } => {
                Self::titled(Kind::Detail, format!("round {round}"), "")
            }
            Fact::Submitted { exec, source } => Self::titled(
                Kind::Detail,
                format!("python · execution {exec}"),
                source.trim_end(),
            ),
            Fact::Completed {
                exec,
                status,
                duration,
                output,
                dropped,
                error,
                background,
            } => {
                let mut text = output.trim_end().to_string();
                let mut add = |part: &str| {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                };
                if *dropped > 0 {
                    add(&format!("({dropped} bytes of output not kept)"));
                }
                if let Some(error) = error {
                    add(error.trim_end());
                }
                for (earlier, output, dropped) in background {
                    let mut part = format!("execution {earlier}, still running, wrote:\n");
                    part.push_str(output.trim_end());
                    if *dropped > 0 {
                        let _ = write!(part, "\n({dropped} bytes of output not kept)");
                    }
                    add(&part);
                }
                Self::titled(
                    Kind::Detail,
                    format!("execution {exec}: {status} in {}", elapsed(*duration)),
                    text,
                )
            }
            Fact::Exec { exec, what } => {
                Self::new(Kind::Detail, format!("execution {exec}: {what}"))
            }
            Fact::RoundEnded {
                round,
                tokens,
                error,
            } => {
                let spent = tokens.map_or_else(String::new, |tokens| format!(" ({tokens} tokens)"));
                match error {
                    Some(error) => Self::new(
                        Kind::Detail,
                        format!("round {round}'s model calls failed{spent}: {error}"),
                    ),
                    None => Self::new(
                        Kind::Detail,
                        format!("round {round}'s model calls ended{spent}"),
                    ),
                }
            }
            Fact::Note { text, inline } => Self::new(
                if *inline { Kind::Note } else { Kind::Detail },
                text.clone(),
            ),
            Fact::Other { title, data } => Self::titled(Kind::Detail, title.clone(), data.clone()),
            Fact::Missed(missed) => Self::new(
                Kind::Detail,
                format!("{missed} of the session's events were lost here, behind the screen"),
            ),
            // The library logs a retry and a move as it makes them, and that
            // log line is the detail view's record of each.
            Fact::Retry { .. }
            | Fact::Failover { .. }
            | Fact::Phase(_)
            | Fact::AgentStarted { .. }
            | Fact::Calling { .. }
            | Fact::Spent(_)
            | Fact::Attempted
            | Fact::Posted
            | Fact::Taken
            | Fact::AgentSent => return None,
        };
        Some(entry)
    }

    fn size(&self) -> usize {
        self.text.len() + self.title.as_ref().map_or(0, String::len)
    }
}

/// The entries, and while the detail view is open their lines at its width,
/// so scrolling does not wrap everything again.
#[derive(Debug, Default)]
pub(super) struct Transcript {
    entries: VecDeque<Entry>,
    bytes: usize,
    dropped: u64,
    wrapped: Option<Wrapped>,
}

#[derive(Debug)]
struct Wrapped {
    width: u16,
    lines: VecDeque<Line<'static>>,
    /// How many of `lines` each entry took, oldest first.
    counts: VecDeque<usize>,
}

impl Wrapped {
    fn push(&mut self, entry: &Entry, theme: Theme) {
        let lines = entry_lines(entry, self.width, theme, false);
        self.counts.push_back(lines.len());
        self.lines.extend(lines);
    }
}

impl Transcript {
    pub(super) fn push(&mut self, entry: Entry, theme: Theme) {
        self.bytes += entry.size();
        if let Some(wrapped) = &mut self.wrapped {
            wrapped.push(&entry, theme);
        }
        self.entries.push_back(entry);
        while self.bytes > BOUND && self.entries.len() > 1 {
            let oldest = self.entries.pop_front().expect("more than one");
            self.bytes -= oldest.size();
            self.dropped += 1;
            if let Some(wrapped) = &mut self.wrapped {
                let count = wrapped.counts.pop_front().unwrap_or(0);
                wrapped.lines.drain(..count.min(wrapped.lines.len()));
            }
        }
    }

    /// How many of the oldest entries were dropped.
    pub(super) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Every entry's lines at `width`, kept until [`Transcript::forget_lines`].
    pub(super) fn lines(&mut self, width: u16, theme: Theme) -> &VecDeque<Line<'static>> {
        if self
            .wrapped
            .as_ref()
            .is_none_or(|wrapped| wrapped.width != width)
        {
            let mut wrapped = Wrapped {
                width,
                lines: VecDeque::new(),
                counts: VecDeque::new(),
            };
            for entry in &self.entries {
                wrapped.push(entry, theme);
            }
            self.wrapped = Some(wrapped);
        }
        &self.wrapped.as_ref().expect("just made").lines
    }

    /// The detail view closed: its lines are not kept, nor made for what
    /// arrives until it opens again.
    pub(super) fn forget_lines(&mut self) {
        self.wrapped = None;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_conversation_shows_all_but_the_agents_working() {
        for kind in [Kind::User, Kind::Agent, Kind::Note, Kind::Error, Kind::Hint] {
            assert!(Entry::new(kind, "x").inline(false), "{kind:?}");
        }
        assert!(!Entry::new(Kind::Detail, "x").inline(false));
        assert!(Entry::new(Kind::Detail, "x").inline(true));
        assert!(!Entry::new(Kind::Trace, "x").inline(true));
    }

    /// The newlines that frame a message are not rows of it.
    #[test]
    fn an_entry_drops_the_newlines_around_it() {
        assert_eq!(Entry::new(Kind::Agent, "\nmessage\n\n").text, "message");
        assert_eq!(Entry::new(Kind::Agent, "one\n\ntwo\n").text, "one\n\ntwo");
        assert_eq!(Entry::new(Kind::Agent, "  spaced  ").text, "  spaced  ");
        assert_eq!(Entry::titled(Kind::Detail, "t", "\n").text, "");
    }

    #[test]
    fn a_log_record_shows_by_its_level() {
        let record = |level| Record {
            level,
            text: "t".into(),
        };
        assert_eq!(Entry::from_record(record(Level::ERROR)).kind, Kind::Error);
        assert_eq!(Entry::from_record(record(Level::WARN)).kind, Kind::Detail);
        assert_eq!(Entry::from_record(record(Level::INFO)).kind, Kind::Trace);
        assert_eq!(Entry::from_record(record(Level::TRACE)).kind, Kind::Trace);
    }

    #[test]
    fn a_completion_shows_its_output_its_error_and_what_ran_beside_it() {
        let entry = Entry::from_fact(&Fact::Completed {
            exec: 4,
            status: "error",
            duration: Duration::from_millis(1_250),
            output: "partial\n".into(),
            dropped: 10,
            error: Some("Traceback ...\nValueError: x\n".into()),
            background: vec![(2, "late\n".into(), 0)],
        })
        .expect("shown");
        assert_eq!(entry.kind, Kind::Detail);
        assert_eq!(entry.title.as_deref(), Some("execution 4: error in 1.2s"));
        assert_eq!(
            entry.text,
            "partial\n(10 bytes of output not kept)\nTraceback ...\nValueError: x\n\
             execution 2, still running, wrote:\nlate"
        );
    }

    #[test]
    fn facts_that_only_move_the_status_line_are_not_recorded() {
        for fact in [
            Fact::Posted,
            Fact::Taken,
            Fact::AgentSent,
            Fact::Attempted,
            Fact::AgentStarted { model: "m".into() },
        ] {
            assert_eq!(Entry::from_fact(&fact), None, "{fact:?}");
        }
        let note = Entry::from_fact(&Fact::Note {
            text: "gone".into(),
            inline: true,
        })
        .expect("shown");
        assert_eq!(note.kind, Kind::Note);
    }

    /// Past the bound, the oldest entries go, and the lines kept for the
    /// detail view go with them.
    #[test]
    fn the_oldest_entries_go_past_the_bound() {
        let mut transcript = Transcript::default();
        let theme = Theme::plain();
        let big = "x".repeat(BOUND / 3);
        transcript.push(Entry::new(Kind::Agent, "first"), theme);
        assert_eq!(transcript.lines(80, theme).len(), 1);
        for _ in 0..3 {
            transcript.push(Entry::new(Kind::Agent, big.clone()), theme);
        }
        assert_eq!(transcript.dropped(), 1);
        assert_eq!(transcript.entries.len(), 3);
        let cached = transcript.lines(80, theme).len();
        assert!(!transcript.lines(80, theme)[0].to_string().contains("first"));
        // Wrapped again once forgotten, the same entries.
        transcript.forget_lines();
        assert_eq!(transcript.lines(80, theme).len(), cached);
    }
}
