//! Where the REPL's input lines come from.
//!
//! [`LineSource`] is the seam between the REPL's dispatch loop and whatever
//! produces lines for it: [`StreamSource`] reads an async stream a line at a
//! time (piped stdin, and every integration test), while
//! [`EditorSource`](super::editor::EditorSource) drives a rustyline editor on
//! `/dev/tty`. The loop is generic over the trait, so slash-command parsing,
//! `/help` composition, and the interrupt state machine are written once and
//! behave identically on both.
//!
//! A source is *lent* the two things it might need -- the loop's stderr sink
//! and a fresh interrupt future -- and decides for itself whether to use them.
//! A source that owns a terminal draws its own prompt there and decodes
//! `Ctrl-C` as a keystroke, so it ignores both; a source reading a stream needs
//! both. That keeps every question about who painted what inside the source,
//! and leaves the loop with no knowledge of terminals at all.

use std::future::Future;

use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines, Stdin,
};

use super::editor::EditorSource;
use crate::error::Result;

/// One line of user input, plus the two non-line outcomes a source reports.
pub(crate) enum LineEvent {
    Line(String),
    /// Ctrl-D on an empty prompt, or the stream ending.
    Eof,
    /// Ctrl-C at the prompt: a keystroke to a terminal-owning source, the
    /// `interrupt` future firing to a stream one.
    Interrupted,
}

/// A source of REPL input lines.
//
// Consumed `&mut self` from the single-task REPL loop and never spawned, so
// the missing `Send` bound on the returned future is deliberate -- the same
// call `init::prompt::PromptSource` makes.
#[allow(async_fn_in_trait)]
pub(crate) trait LineSource {
    /// Show `prompt` and read one line.
    ///
    /// `out` is the loop's stderr sink, for a source that has nowhere better to
    /// draw; `interrupt` completes when a SIGINT arrives. A source that handles
    /// either itself simply does not use the one it does not need.
    async fn read_line<E, F>(
        &mut self,
        prompt: &str,
        out: &mut E,
        interrupt: F,
    ) -> Result<LineEvent>
    where
        E: AsyncWrite + Unpin,
        F: Future<Output = ()>;
}

/// Line-at-a-time reads over an async stream: piped stdin, and every test.
/// The terminal, if there is one, stays in canonical mode -- this is what the
/// REPL did before rustyline.
pub(crate) struct StreamSource<RD> {
    lines: Lines<RD>,
}

impl<RD: AsyncBufRead + Unpin> StreamSource<RD> {
    pub(crate) fn new(stdin: RD) -> Self {
        Self {
            lines: stdin.lines(),
        }
    }
}

impl<RD: AsyncBufRead + Unpin> LineSource for StreamSource<RD> {
    async fn read_line<E, F>(
        &mut self,
        prompt: &str,
        out: &mut E,
        interrupt: F,
    ) -> Result<LineEvent>
    where
        E: AsyncWrite + Unpin,
        F: Future<Output = ()>,
    {
        out.write_all(prompt.as_bytes()).await?;
        out.flush().await?;

        let event = tokio::select! {
            line = self.lines.next_line() => match line? {
                Some(line) => LineEvent::Line(line),
                None => LineEvent::Eof,
            },
            _ = interrupt => LineEvent::Interrupted,
        };

        // Only a typed line ends with the user's own newline. Ctrl-C and EOF
        // leave the prompt unfinished, so close it here.
        if !matches!(event, LineEvent::Line(_)) {
            out.write_all(b"\n").await?;
            out.flush().await?;
        }
        Ok(event)
    }
}

/// The source [`auto`] picked.
///
/// An enum rather than a `Box<dyn LineSource>` for the reason
/// [`AutoPrompt`](crate::init::prompt::AutoPrompt) is one: `async fn`-in-trait
/// is not dyn-compatible. Forwarding by hand gives one concrete type, no
/// allocation, and no type erasure.
pub(crate) enum AutoSource {
    Editor(EditorSource),
    Stream(StreamSource<BufReader<Stdin>>),
}

impl LineSource for AutoSource {
    async fn read_line<E, F>(
        &mut self,
        prompt: &str,
        out: &mut E,
        interrupt: F,
    ) -> Result<LineEvent>
    where
        E: AsyncWrite + Unpin,
        F: Future<Output = ()>,
    {
        match self {
            Self::Editor(source) => source.read_line(prompt, out, interrupt).await,
            Self::Stream(source) => source.read_line(prompt, out, interrupt).await,
        }
    }
}

/// An editor whenever the terminal can drive one, today's line-at-a-time reader
/// otherwise -- which is what keeps `echo "..." | outrig run` working.
pub(crate) fn auto() -> AutoSource {
    match EditorSource::try_new() {
        Some(editor) => AutoSource::Editor(editor),
        None => AutoSource::Stream(StreamSource::new(BufReader::new(tokio::io::stdin()))),
    }
}
