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
//!
//! A live object a host program injects is announced the same way, to each
//! child made before it, in the next result: a line of its own, which opens
//! no round, and is made again if the round that made it failed
//! ([`Injected`]).

use std::collections::BTreeMap;
use std::error::Error;
use std::future::Future;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use tokio::sync::Mutex;

use super::AgentError;
use super::orientation;
use crate::harness::bindings::Brief;
use crate::harness::skills::Catalog;
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
    injected: Injected,
}

/// The briefs of the live objects a host program injected into a session, in order: what an
/// agent made later is oriented with, and an agent made earlier is told of.
#[derive(Clone, Default)]
pub(crate) struct Injected(Arc<std::sync::Mutex<Vec<Brief>>>);

impl Injected {
    pub(crate) fn push(&self, brief: Brief) {
        self.lock().push(brief);
    }

    pub(crate) fn briefs(&self) -> Vec<Brief> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Brief>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
/// told of; and how many of the live objects injected, counting those it was
/// made knowing.
#[derive(Default)]
struct Told {
    /// By the round in flight, or the last one.
    announced: AtomicU64,
    /// By the last round that ended well.
    kept: AtomicU64,
    /// The same, of the messages alone, by channel: a child's channels carry
    /// requests too, whose deliveries are no news of a message, and what one
    /// channel delivered is no news of what waits on another.
    messages: std::sync::Mutex<Messages>,
    /// Live objects, by the round in flight, or the last one.
    injections: AtomicUsize,
    /// Live objects, by the last round that ended well.
    injections_kept: AtomicUsize,
}

/// How many messages each of a child's channels delivered that the model has
/// been told of.
#[derive(Default)]
struct Messages {
    /// By the round in flight, or the last one.
    announced: BTreeMap<String, u64>,
    /// By the last round that ended well.
    kept: BTreeMap<String, u64>,
}

impl Told {
    fn messages(&self) -> std::sync::MutexGuard<'_, Messages> {
        self.messages.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Announcer {
    pub(crate) fn new(interpreter: Interpreter) -> Self {
        Self {
            source: Source::Interpreter(interpreter),
            told: Arc::default(),
            injected: Injected::default(),
        }
    }

    /// A child's announcer, counting its requests by channel as `counts`
    /// gives them, and telling of each live object `injected` holds from now on.
    pub(crate) fn of_requests(
        counts: Arc<dyn Fn() -> Channels + Send + Sync>,
        injected: Injected,
    ) -> Self {
        let known = injected.lock().len();
        let told = Told {
            injections: known.into(),
            injections_kept: known.into(),
            ..Told::default()
        };
        Self {
            source: Source::Requests(counts),
            told: Arc::new(told),
            injected,
        }
    }

    /// The line for each live object injected since the round in flight last
    /// told of one, counted as announced.
    pub(crate) fn injections(&self) -> Vec<String> {
        let injected = self.injected.lock();
        let from = self.told.injections.swap(injected.len(), Ordering::SeqCst);
        injected[from..].iter().map(orientation::injected).collect()
    }

    /// A child's round opens, telling the model what waits as `channels`
    /// counted it, the counts the opening's words were read from: only what
    /// arrives from there on is news within it. A live object a failed round
    /// told of is told of again.
    pub(crate) fn opened_at(&self, channels: &Channels) {
        let kept = self.told.injections_kept.load(Ordering::SeqCst);
        self.told.injections.store(kept, Ordering::SeqCst);
        if let Source::Requests(_) = &self.source {
            self.told
                .announced
                .store(delivered(channels), Ordering::SeqCst);
            self.told.messages().announced = messages(channels).collect();
        }
    }

    /// Whether `channels`, a child's counts as its announcer reads them, hold a
    /// message the model has not been told of, by a round that ended well or
    /// one that was refused: a channel that delivered a message past what it
    /// was told, and holds one unread. A channel is read in order, so what it
    /// holds unread then includes the message last delivered. What opens a
    /// round for a message, which owes no reply and so never enters the rule
    /// for requests; a request is no news of one.
    pub(crate) fn unannounced(&self, channels: &Channels) -> bool {
        let told = self.told.messages();
        channels.iter().any(|(channel, counts)| {
            !counts.requests
                && counts.pending > 0
                && counts.delivered > told.kept.get(channel).copied().unwrap_or(0)
        })
    }

    /// A child's round the provider refused for what it carried, opened on
    /// `opening`, the counts its opening's words were read from: what the
    /// opening told is kept, so it opens no round again -- the same opening
    /// would be refused the same way. What only a result told of is not: the
    /// model never took that result, so a message it told of opens the next
    /// round. A live object it told of is told of again.
    pub(crate) fn settle(&self, opening: &Channels) {
        self.told
            .kept
            .fetch_max(delivered(opening), Ordering::SeqCst);
        raise(&mut self.told.messages().kept, messages(opening));
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

    /// The round in flight ended well, so what it announced, of every channel
    /// and of the messages alone, was read.
    pub(crate) fn keep(&self) {
        let told = &self.told;
        told.kept
            .fetch_max(told.announced.load(Ordering::SeqCst), Ordering::SeqCst);
        {
            let mut messages = told.messages();
            let announced = messages.announced.clone();
            raise(&mut messages.kept, announced);
        }
        told.injections_kept
            .fetch_max(told.injections.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// What waits unread, if anything was delivered past `seen`, counted as
    /// announced.
    async fn news(&self, seen: &AtomicU64) -> Result<Option<String>, AgentError> {
        let channels = match &self.source {
            Source::Interpreter(interpreter) => {
                tokio::time::timeout(PENDING_TIMEOUT, interpreter.pending())
                    .await
                    .map_err(|_| AgentError::Unanswered(PENDING_TIMEOUT))??
            }
            Source::Requests(counts) => counts(),
        };
        let delivered = delivered(&channels);
        if delivered <= seen.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.told.announced.fetch_max(delivered, Ordering::SeqCst);
        raise(&mut self.told.messages().announced, messages(&channels));
        Ok(announcement(&channels))
    }
}

/// Every item ever delivered to `channels`.
fn delivered(channels: &Channels) -> u64 {
    channels.values().map(|counts| counts.delivered).sum()
}

/// Every message ever delivered to each of `channels` that carries messages
/// rather than requests.
fn messages(channels: &Channels) -> impl Iterator<Item = (String, u64)> + '_ {
    channels
        .iter()
        .filter(|(_, counts)| !counts.requests)
        .map(|(channel, counts)| (channel.clone(), counts.delivered))
}

/// Each count of `told` raised to the channel's in `to`, where higher.
fn raise(told: &mut BTreeMap<String, u64>, to: impl IntoIterator<Item = (String, u64)>) {
    for (channel, count) in to {
        let at = told.entry(channel).or_default();
        *at = (*at).max(count);
    }
}

/// What waits, by channel and count: `2 messages are waiting on
/// runtime.channels["user"]`, or requests on a child's request channels.
/// `None` when nothing does.
pub(crate) fn announcement(channels: &Channels) -> Option<String> {
    let waiting: Vec<String> = channels
        .iter()
        .filter(|(_, counts)| counts.pending > 0)
        .map(|(channel, counts)| {
            if channel == crate::harness::QUESTION_CHANNEL {
                return format!(
                    "{} question handle(s) settled; inspect them with await or result()",
                    counts.pending
                );
            }
            let words = match (counts.requests, counts.pending) {
                (false, 1) => "message is",
                (false, _) => "messages are",
                (true, 1) => "request is",
                (true, _) => "requests are",
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
    /// The session's skills, which a directive names one of.
    skills: Option<Arc<Catalog>>,
}

impl UserChannel {
    /// The one end for `interpreter`'s agent, whose directives name skills of
    /// `skills`. Built once: it takes over the interpreter's only
    /// subscription.
    pub(crate) fn new(interpreter: Interpreter, skills: Option<Arc<Catalog>>) -> Self {
        let sent = interpreter.subscribe();
        Self {
            interpreter,
            sent: Arc::new(Mutex::new(sent)),
            skills,
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

    /// Send the agent `line`, a `/name` directive the user typed, naming the
    /// session's skill `skill`.
    ///
    /// The agent's `runtime.channels["user"]` delivers it as a message whose
    /// `.skill` is `skill`, which asks the agent to run the skill; nothing of
    /// the skill runs here. For a skill with a `skill.py`, the message is
    /// `line`, and it is on its way when this returns, as
    /// [`UserChannel::send`]'s is. For one without, it is the skill's
    /// instructions -- its `SKILL.md` past the frontmatter, read through its
    /// source -- a blank line, then `line`, and it is on its way once the
    /// future has read them. The future says what `send`'s does, or that the
    /// session serves no skill named `skill`, or why its instructions could
    /// not be read; nothing is sent then.
    pub fn send_directive(
        &self,
        skill: &str,
        line: &str,
    ) -> impl Future<Output = Result<usize, Box<dyn Error + Send + Sync>>> + Send + 'static {
        let catalog = self.skills.clone();
        let found = catalog.as_ref().and_then(|catalog| catalog.directed(skill));
        let queued = match &found {
            Some((source, true)) => {
                Some(self.interpreter.post_directive(USER, line, skill, source))
            }
            _ => None,
        };
        let (interpreter, skill, line) = (
            self.interpreter.clone(),
            skill.to_string(),
            line.to_string(),
        );
        async move {
            let (Some(catalog), Some((source, _))) = (catalog, found) else {
                return Err(
                    format!("unknown-skill: the session serves no skill named {skill:?}").into(),
                );
            };
            if let Some(queued) = queued {
                return Ok(queued?.await?);
            }
            let instructions = catalog.read_instructions(&skill).await?;
            let instructions = instructions.trim_start_matches('\n').trim_end();
            let body = if instructions.is_empty() {
                line
            } else {
                format!("{instructions}\n\n{line}")
            };
            Ok(interpreter
                .post_directive(USER, &body, &skill, &source)?
                .await?)
        }
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{Announcer, Injected};
    use crate::python::host::{Channels, Counts};

    fn counts(channels: &[(&str, usize, u64, bool)]) -> Channels {
        channels
            .iter()
            .map(|&(name, pending, delivered, requests)| {
                (
                    name.to_string(),
                    Counts {
                        pending,
                        delivered,
                        requests,
                    },
                )
            })
            .collect()
    }

    /// A message announced and left unread is no news again because a request arrived and was
    /// answered after the round's last result: news of a message is counted in messages.
    #[test]
    fn a_request_is_no_news_of_a_message() {
        let now = Arc::new(Mutex::new(counts(&[("inbox", 1, 1, false)])));
        let read = Arc::clone(&now);
        let announcer = Announcer::of_requests(
            Arc::new(move || read.lock().unwrap().clone()),
            Injected::default(),
        );
        let opening = now.lock().unwrap().clone();
        assert!(announcer.unannounced(&opening));
        announcer.opened_at(&opening);
        announcer.keep();
        assert!(!announcer.unannounced(&opening), "announced, and kept");
        let answered = counts(&[("inbox", 1, 1, false), ("fizz", 0, 1, true)]);
        assert!(
            !announcer.unannounced(&answered),
            "a request taken and answered is no news of the message"
        );
        let arrived = counts(&[("inbox", 2, 2, false), ("fizz", 0, 1, true)]);
        assert!(announcer.unannounced(&arrived), "a second message is");
    }

    /// A message announced and left unread on one channel is no news again because another
    /// channel delivered a message the agent's code then read: news is counted per channel.
    #[test]
    fn one_channels_message_is_no_news_of_anothers() {
        let opening = counts(&[("a", 0, 0, false), ("b", 1, 1, false)]);
        let announcer = Announcer::of_requests(Arc::new(Channels::default), Injected::default());
        assert!(announcer.unannounced(&opening));
        announcer.opened_at(&opening);
        announcer.keep();
        let read = counts(&[("a", 0, 1, false), ("b", 1, 1, false)]);
        assert!(
            !announcer.unannounced(&read),
            "a's message was read, and b's was told of"
        );
        let waiting = counts(&[("a", 1, 1, false), ("b", 1, 1, false)]);
        assert!(announcer.unannounced(&waiting), "a's message waits unread");
    }

    /// A refused round keeps what its opening told and no more: a message only a result told
    /// of, the call carrying that result refused, is news; the round it opens, refused too,
    /// keeps it, so no round opens on it again.
    #[tokio::test]
    async fn a_refused_round_keeps_only_its_opening() {
        let opening = counts(&[("inbox", 1, 1, false)]);
        let now = Arc::new(Mutex::new(opening.clone()));
        let read = Arc::clone(&now);
        let announcer = Announcer::of_requests(
            Arc::new(move || read.lock().unwrap().clone()),
            Injected::default(),
        );
        announcer.opened_at(&opening);
        let arrived = counts(&[("inbox", 1, 2, false)]);
        *now.lock().unwrap() = arrived.clone();
        assert!(announcer.arrived().await.expect("counted").is_some());
        announcer.settle(&opening);
        assert!(
            announcer.unannounced(&arrived),
            "told only in a result the model never took"
        );
        announcer.opened_at(&arrived);
        announcer.settle(&arrived);
        assert!(
            !announcer.unannounced(&arrived),
            "told in a refused opening"
        );
    }
}
