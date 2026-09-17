//! The session's one writer to the terminal.
//!
//! Two things reach the user during a turn and they arrive from different tasks: the model's
//! own prose, at the end of a turn, and a message Python sent on the `user` channel, which can
//! arrive at any moment -- including from a background task long after the turn that started
//! it returned.
//!
//! Both used to write stdout directly, from different tasks through different handles. That
//! interleaves: the REPL emits a reply as two awaited `write_all`s (the text, then its newline)
//! and the runtime advances the kernel's driver task at exactly those await points, so a send
//! landing there splits a reply from its own newline. Routing both through one lock, a whole
//! message at a time, makes the worst case a question of which message comes first rather than
//! a corrupted one.
//!
//! The split across streams is [`crate::repl`]'s existing rule: what the agent said goes to
//! stdout, everything framing it goes to stderr, so `outrig run > out.txt` collects the agent's
//! words and nothing else.

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::error::Result;

/// Marks a message Python sent, so it is not mistaken for the log traffic around it. On stderr,
/// because it is framing rather than something the agent said.
const AGENT_LABEL: &str = "\n[outrig] agent message:\n";

pub struct Console {
    streams: Mutex<Streams>,
    /// How many messages Python has sent. A turn that raised this said something to the user,
    /// even if the model's own reply came back blank -- which is what stops such a turn being
    /// reported as one that produced nothing.
    sent: AtomicU64,
}

struct Streams {
    out: Box<dyn AsyncWrite + Send + Unpin>,
    err: Box<dyn AsyncWrite + Send + Unpin>,
}

impl Console {
    /// The real terminal.
    pub fn new() -> Self {
        Self::with_streams(tokio::io::stdout(), tokio::io::stderr())
    }

    /// Write somewhere else. Tests pass `tokio::io::duplex` halves to read back what landed on
    /// which stream.
    pub fn with_streams(
        out: impl AsyncWrite + Send + Unpin + 'static,
        err: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Self {
        Self {
            streams: Mutex::new(Streams {
                out: Box::new(out),
                err: Box::new(err),
            }),
            sent: AtomicU64::new(0),
        }
    }

    /// How many messages Python has sent so far this session. Compared across a turn to learn
    /// whether that turn spoke.
    pub fn messages_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// A message Python sent on the `user` channel: a result, an answer, or a notification from
    /// work that finished on its own. Labeled, because unlike the model's prose it has no fixed
    /// place in the turn to identify it.
    pub async fn agent_message(&self, text: &str) -> Result<()> {
        self.sent.fetch_add(1, Ordering::Relaxed);
        let mut streams = self.streams.lock().await;
        streams.err.write_all(AGENT_LABEL.as_bytes()).await?;
        streams.err.flush().await?;
        streams.write_body(text).await
    }

    /// The model's own prose, at the end of a turn.
    pub async fn reply(&self, text: &str) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.streams.lock().await.write_body(text).await
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Streams {
    /// One message, one flush, while the lock is held -- so nothing can land inside it.
    async fn write_body(&mut self, text: &str) -> Result<()> {
        self.out.write_all(text.as_bytes()).await?;
        if !text.ends_with('\n') {
            self.out.write_all(b"\n").await?;
        }
        self.out.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives `body` against a console whose streams are in-memory, and hands back what each
    /// one received.
    async fn captured<F, Fut>(body: F) -> (String, String)
    where
        F: FnOnce(Console) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (out_tx, mut out_rx) = tokio::io::duplex(4096);
        let (err_tx, mut err_rx) = tokio::io::duplex(4096);
        let console = Console::with_streams(out_tx, err_tx);
        body(console).await;
        let mut out = Vec::new();
        let mut err = Vec::new();
        // The write halves are dropped with the console, so both reads see EOF.
        tokio::io::AsyncReadExt::read_to_end(&mut out_rx, &mut out)
            .await
            .expect("stdout half readable");
        tokio::io::AsyncReadExt::read_to_end(&mut err_rx, &mut err)
            .await
            .expect("stderr half readable");
        (
            String::from_utf8(out).expect("utf-8"),
            String::from_utf8(err).expect("utf-8"),
        )
    }

    #[tokio::test]
    async fn an_agent_message_is_labeled_on_stderr_and_bodied_on_stdout() {
        let (out, err) = captured(|console| async move {
            console
                .agent_message("the downloads are complete")
                .await
                .expect("the write succeeds");
        })
        .await;
        assert_eq!(out, "the downloads are complete\n");
        // The label must not reach stdout: `outrig run > out.txt` collects what the agent
        // said, and this is outrig talking about the agent.
        assert_eq!(err, AGENT_LABEL);
    }

    #[tokio::test]
    async fn a_reply_is_unlabeled() {
        let (out, err) = captured(|console| async move {
            console.reply("I read your message.").await.expect("ok");
        })
        .await;
        assert_eq!(out, "I read your message.\n");
        assert!(err.is_empty(), "a reply frames nothing: {err:?}");
    }

    #[tokio::test]
    async fn a_body_that_ends_in_a_newline_does_not_get_a_second_one() {
        let (out, _) = captured(|console| async move {
            console.reply("already terminated\n").await.expect("ok");
            console.agent_message("so is this\n").await.expect("ok");
        })
        .await;
        assert_eq!(out, "already terminated\nso is this\n");
    }

    /// The REPL treats an empty reply as "nothing to print"; keeping that here stops a silent
    /// turn from putting a stray blank line under the report that explained it.
    #[tokio::test]
    async fn an_empty_reply_writes_nothing() {
        let (out, err) = captured(|console| async move {
            console.reply("").await.expect("ok");
        })
        .await;
        assert!(out.is_empty(), "got: {out:?}");
        assert!(err.is_empty(), "got: {err:?}");
    }

    /// What a turn consults to learn whether it spoke. Only `agent_message` counts -- a
    /// `reply` is the model's own text, which the turn already knows about.
    #[tokio::test]
    async fn only_agent_messages_are_counted() {
        let (out_tx, _out_rx) = tokio::io::duplex(4096);
        let (err_tx, _err_rx) = tokio::io::duplex(4096);
        let console = Console::with_streams(out_tx, err_tx);

        assert_eq!(console.messages_sent(), 0);
        console.reply("narration").await.expect("ok");
        assert_eq!(console.messages_sent(), 0, "a reply is not a channel send");

        console.agent_message("first").await.expect("ok");
        console.agent_message("second").await.expect("ok");
        assert_eq!(console.messages_sent(), 2);
    }

    /// The whole point of the lock: a message is written as a unit, so a concurrent writer
    /// cannot land between a body and its newline.
    #[tokio::test]
    async fn concurrent_writers_do_not_split_each_other() {
        let (out, _) = captured(|console| async move {
            let console = std::sync::Arc::new(console);
            let mut writers = Vec::new();
            for i in 0..16 {
                let console = console.clone();
                writers.push(tokio::spawn(async move {
                    if i % 2 == 0 {
                        console.reply(&format!("reply-{i}")).await
                    } else {
                        console.agent_message(&format!("send-{i}")).await
                    }
                }));
            }
            for writer in writers {
                writer.await.expect("the task ran").expect("the write succeeds");
            }
            std::sync::Arc::into_inner(console).expect("the last reference");
        })
        .await;
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort_unstable();
        let mut expected: Vec<String> = (0..16)
            .map(|i| {
                if i % 2 == 0 {
                    format!("reply-{i}")
                } else {
                    format!("send-{i}")
                }
            })
            .collect();
        expected.sort();
        assert_eq!(lines, expected, "a message was split or lost: {out:?}");
    }
}
