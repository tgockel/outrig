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

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Stdin};

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
///
/// Lines are decoded lossily: a byte sequence that is not UTF-8 becomes
/// U+FFFD and the line is still sent, rather than failing the read and with it
/// the session.
pub(crate) struct StreamSource<RD> {
    stdin: RD,
    /// The line being read. A field rather than a local because `read_until`
    /// is cancel safe only over a buffer that outlives the call: an interrupt
    /// mid-line keeps the bytes already read, as `Lines::next_line` did.
    pending: Vec<u8>,
}

impl<RD: AsyncBufRead + Unpin> StreamSource<RD> {
    pub(crate) fn new(stdin: RD) -> Self {
        Self {
            stdin,
            pending: Vec::new(),
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

        // Split on the raw byte: 0x0A never occurs inside a multi-byte UTF-8
        // sequence, so no character straddles two lines.
        let event = tokio::select! {
            read = self.stdin.read_until(b'\n', &mut self.pending) => {
                read?;
                // Empty, not `read == 0`: a cancelled read may have left the
                // start of an unterminated last line behind.
                if self.pending.is_empty() {
                    LineEvent::Eof
                } else {
                    // The terminator stays on: the loop trims line endings
                    // for every source.
                    let line = std::mem::take(&mut self.pending);
                    LineEvent::Line(String::from_utf8(line).unwrap_or_else(|e| {
                        String::from_utf8_lossy(e.as_bytes()).into_owned()
                    }))
                }
            }
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

#[cfg(test)]
mod tests {
    use tokio::io::duplex;

    use super::*;

    /// An interrupt mid-line must not lose what was already read: the reader
    /// has consumed those bytes from the stream, so a buffer that died with the
    /// cancelled call would hand the next read only the line's tail.
    #[tokio::test]
    async fn an_interrupt_mid_line_keeps_the_bytes_already_read() {
        let (mut stdin_w, stdin_r) = duplex(64);
        let mut source = StreamSource::new(BufReader::new(stdin_r));
        let mut out = Vec::new();

        stdin_w.write_all(b"hel").await.unwrap();
        // Pending on its first poll, so the read is polled -- and takes `hel`
        // -- before the interrupt fires.
        let event = source
            .read_line("> ", &mut out, tokio::task::yield_now())
            .await
            .expect("read");
        assert!(matches!(event, LineEvent::Interrupted));

        stdin_w.write_all(b"lo\n").await.unwrap();
        let event = source
            .read_line("> ", &mut out, std::future::pending::<()>())
            .await
            .expect("read");
        match event {
            LineEvent::Line(line) => assert_eq!(line, "hello\n"),
            _ => panic!("expected a line"),
        }
    }
}
