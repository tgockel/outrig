//! The agent's `user` channel from the host's end, and what the model has been
//! told of what waits on it.
//!
//! A message the user sends is queued in the interpreter, and the model hears
//! of it by name and count -- never by what it says. Reading it is an act the
//! agent's code takes, which is what lets a model be told that three messages
//! wait without any of them entering its context.
//!
//! The model is told when a round opens and, for a message arriving while one
//! runs, in the next result `submit_python` hands back. What counts as new is
//! the interpreter's to say: each channel counts every message ever delivered
//! to it, and [`Announcer`] keeps only how far along those counts the model has
//! been told -- provisionally within a round, and for good once a round ends
//! well. So an announcement a failed or dropped round made, which the model may
//! never have read, is made again.
//!
//! A child agent's channels carry requests, and the host keeps their record
//! itself, so a child's announcer counts from that ([`Announcer::of_requests`])
//! and says "requests". When a child's round opens is the runner's to decide;
//! its announcer only says what arrives while one runs.

use std::error::Error;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use super::AgentError;
use crate::python::host::{Channels, Interpreter, Sent, USER};

/// How long the interpreter has to say what waits. It answers beside the
/// agent's loop, as it answers a CPU reading, so only an interpreter nothing
/// can run in -- native code holding it -- takes anywhere near this.
const PENDING_TIMEOUT: Duration = Duration::from_secs(2);

/// What the model has been told of the messages delivered to the agent's
/// channels. Shared by each round and the tool.
#[derive(Clone)]
pub(crate) struct Announcer {
    source: Source,
    told: Arc<Told>,
}

/// Where an announcer's counts come from.
#[derive(Clone)]
enum Source {
    /// The interpreter's own, for the primary's channels.
    Interpreter(Interpreter),
    /// The host's record of a child's requests, by channel.
    Requests(Arc<dyn Fn() -> Channels + Send + Sync>),
}

/// How many deliveries, summed over the agent's channels, the model has been
/// told of.
#[derive(Default)]
struct Told {
    /// By the round in flight, or the last one.
    announced: AtomicU64,
    /// By the last round that ended well.
    kept: AtomicU64,
}

impl Announcer {
    pub(crate) fn new(interpreter: Interpreter) -> Self {
        Self {
            source: Source::Interpreter(interpreter),
            told: Arc::default(),
        }
    }

    /// A child's announcer, counting its requests by channel as `counts`
    /// gives them.
    pub(crate) fn of_requests(counts: Arc<dyn Fn() -> Channels + Send + Sync>) -> Self {
        Self {
            source: Source::Requests(counts),
            told: Arc::default(),
        }
    }

    /// A child's round opens, telling the model what waits: only what arrives
    /// from here on is news within it.
    pub(crate) fn opened(&self) {
        if let Source::Requests(counts) = &self.source {
            let delivered = counts().values().map(|counts| counts.delivered).sum();
            self.told.announced.store(delivered, Ordering::SeqCst);
        }
    }

    /// What a round opens on: what waits, if anything arrived since a round
    /// that ended well last said. `None` when nothing did, or when the agent's
    /// code has already read what did -- there is then nothing to tell.
    pub(crate) async fn opening(&self) -> Result<Option<String>, AgentError> {
        let news = self.news(&self.told.kept).await?;
        if news.is_none() {
            self.keep();
        }
        Ok(news)
    }

    /// What waits, if anything arrived since the round in flight last said.
    pub(crate) async fn arrived(&self) -> Result<Option<String>, AgentError> {
        self.news(&self.told.announced).await
    }

    /// The round in flight ended well, so what it announced was read.
    pub(crate) fn keep(&self) {
        let announced = self.told.announced.load(Ordering::SeqCst);
        self.told.kept.fetch_max(announced, Ordering::SeqCst);
    }

    /// What waits unread, if anything was delivered past `seen`, counted as
    /// announced.
    async fn news(&self, seen: &AtomicU64) -> Result<Option<String>, AgentError> {
        let (channels, noun) = match &self.source {
            Source::Interpreter(interpreter) => (
                tokio::time::timeout(PENDING_TIMEOUT, interpreter.pending())
                    .await
                    .map_err(|_| AgentError::Unanswered(PENDING_TIMEOUT))??,
                Noun::Message,
            ),
            Source::Requests(counts) => (counts(), Noun::Request),
        };
        let delivered = channels.values().map(|counts| counts.delivered).sum();
        if delivered <= seen.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.told.announced.fetch_max(delivered, Ordering::SeqCst);
        Ok(announcement(&channels, noun))
    }
}

/// What waits on a channel: a message, or a request.
#[derive(Clone, Copy)]
pub(crate) enum Noun {
    Message,
    Request,
}

/// What waits, by channel and count: `2 messages are waiting on
/// runtime.channels["user"]`, or requests on a child's. `None` when nothing
/// does.
pub(crate) fn announcement(channels: &Channels, noun: Noun) -> Option<String> {
    let waiting: Vec<String> = channels
        .iter()
        .filter(|(_, counts)| counts.pending > 0)
        .map(|(channel, counts)| {
            let words = match (noun, counts.pending) {
                (Noun::Message, 1) => "message is",
                (Noun::Message, _) => "messages are",
                (Noun::Request, 1) => "request is",
                (Noun::Request, _) => "requests are",
            };
            format!(
                "{} {words} waiting on runtime.channels[{channel:?}]",
                counts.pending
            )
        })
        .collect();
    (!waiting.is_empty()).then(|| waiting.join("; "))
}

/// The user's end of an agent's `user` channel: what the user sends goes to
/// the agent's `runtime.channels["user"]`, and what the agent sends there
/// comes out here. [`Session::user_channel`](crate::harness::Session::user_channel)
/// hands it out.
///
/// Clones share one channel. A message the agent sends reaches one
/// [`UserChannel::receive`], whichever clone it is called on.
#[derive(Clone)]
pub struct UserChannel {
    interpreter: Interpreter,
    sent: Arc<Mutex<Sent>>,
}

impl UserChannel {
    /// The one end for `interpreter`'s agent. Built once: it takes over the
    /// interpreter's only subscription.
    pub(crate) fn new(interpreter: Interpreter) -> Self {
        let sent = interpreter.subscribe();
        Self {
            interpreter,
            sent: Arc::new(Mutex::new(sent)),
        }
    }

    /// Send `text` to the agent.
    ///
    /// The message is on its way when this returns, ahead of anything sent
    /// after it, whether or not the future is awaited. The future says whether
    /// the channel took it -- a full channel does not, and neither does the
    /// interpreter once it has exited -- and how many messages then waited
    /// unread, this one included.
    ///
    /// Sending does not start a round:
    /// [`Session::round`](crate::harness::Session::round) does, and tells the
    /// model what waits.
    pub fn send(
        &self,
        text: &str,
    ) -> impl Future<Output = Result<usize, Box<dyn Error + Send + Sync>>> + Send + 'static {
        let queued = self.interpreter.post(USER, text);
        async move { Ok(queued?.await?) }
    }

    /// The next message the agent sent, waiting until there is one: text it
    /// passed to `runtime.channels["user"].send(...)`, whether from a round's
    /// code or from a task still running after the round ended. `None` once
    /// the interpreter has exited and every message it sent has been received.
    ///
    /// The agent runs at most 16 messages ahead of this: past that, its code
    /// waits in `send` until one is received. So a caller should receive while
    /// a round runs -- one that receives only once
    /// [`Session::round`](crate::harness::Session::round) returns can leave
    /// that round's code waiting on it.
    pub async fn receive(&self) -> Option<String> {
        self.sent.lock().await.recv().await
    }

    /// Every message the agent has sent that is waiting to be received now,
    /// in order, without waiting for more -- what to show before leaving.
    /// Messages a task goes on sending after this are not waited for.
    ///
    /// It never waits, not even behind a [`UserChannel::receive`] outstanding
    /// on a clone: that receive takes each message as it arrives, so none is
    /// waiting, and this returns nothing.
    pub fn receive_waiting(&self) -> Vec<String> {
        self.sent
            .try_lock()
            .map(|mut sent| sent.recv_waiting())
            .unwrap_or_default()
    }
}
