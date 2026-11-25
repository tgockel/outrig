//! External channels end to end (`0003-51`): a Rust owner declares a channel with a definition
//! and a backend, pushes items into it, and the agent holding it receives and sends there, its
//! rounds opened by its runner as the channel's wake policy says. Each agent's model is the
//! routed mock, its code run on the host's interpreter; the backend is the test's own, which
//! records what it is given and answers as the test says.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::children_tests::{
    Tree, Watch, after_result, data, data_of, exec_outputs, kind, of_agent, opening, raised,
    serving_each, tree_watched,
};
use super::instance_tests::{launching, past_launch, ready_then};
use super::mock_http;
use super::mock_http::{
    RecordedRequest, Responder, Style, scripted, submit, system_text, text_reply,
};
use super::root_tests::agent_of;
use crate::harness::event::ItemOutcome;
use crate::harness::event::MessageId;
use crate::harness::skills::DirectorySource;
use crate::harness::{
    AgentState, ApplicationSpec, Argument, BackendFuture, ChannelBackend, ChannelDefinition,
    ChannelOperation, ConstructSpec, Consumed, Contract, DeliverError, Delivered, DeliveryId,
    Facility, Inbound, ItemKind, Outbound, Position, Positions, Query, RootHandle, SendAnswer,
    SessionBuilder, SessionError, Settled, Verdict, Wake, WithdrawAnswer, Withdrawal,
};
use crate::python::testing::{Flag, py, until, within};

/// The application's `app` package: `app.messages` holds the type a typed channel names, and
/// `app.agents` the classes the tests construct.
const MESSAGES: &str = r#"
import dataclasses
import threading

# The thread each `Note` was built on, in order.
BUILT = []


@dataclasses.dataclass
class Note:
    text: str
    level: int

    def __post_init__(self):
        BUILT.append(threading.current_thread().name)
"#;

const AGENTS: &str = r#"
import outrig


class Inbox(outrig.Agent):
    """ROLE inbox. You answer what arrives on your channels."""

    def __init__(self, facilities=None, *, model=None):
        super().__init__(facilities=facilities, model=model)


class Helper(outrig.Agent):
    """ROLE helper. You answer pings."""

    def __init__(self, facilities=None, *, model=None):
        super().__init__(facilities=facilities, model=model)

    @outrig.agent
    async def ping(self, text: str) -> str:
        """Answer it."""
        ...
"#;

/// The application holding [`MESSAGES`] and [`AGENTS`], imported at start, in a directory kept
/// while the test runs.
fn application() -> (tempfile::TempDir, ApplicationSpec) {
    let dir = tempfile::tempdir().expect("an application directory");
    let package = dir.path().join("app");
    std::fs::create_dir_all(&package).expect("the package's directory");
    std::fs::write(package.join("__init__.py"), "").expect("write __init__.py");
    std::fs::write(package.join("messages.py"), MESSAGES).expect("write messages.py");
    std::fs::write(package.join("agents.py"), AGENTS).expect("write agents.py");
    let spec = ApplicationSpec::directory(dir.path()).imports(["app.agents", "app.messages"]);
    (dir, spec)
}

/// How the test backend answers the next send.
#[derive(Debug, Clone)]
pub(super) enum Answer {
    Accept,
    /// Accepted, once the test lets it be.
    After(Arc<tokio::sync::Notify>),
    NotAccept(&'static str),
    /// Uncertain, the item lost before it was stored.
    Uncertain(&'static str),
    /// Uncertain, the item stored: its answer was what was lost.
    UncertainKept(&'static str),
    /// Never: the call runs past its bound, the item never stored.
    Never,
    /// Never, the item stored before the call stalled.
    NeverKept,
}

/// The test's far end of every channel: what it was sent and told, and how it answers.
#[derive(Default)]
pub(super) struct Backend {
    sent: Mutex<Vec<Outbound>>,
    notices: Mutex<Vec<Consumed>>,
    /// How each next send is answered; once these run out, accepted.
    answers: Mutex<VecDeque<Answer>>,
    /// Whether a consumed notice fails, as an unreachable store's would.
    pub(super) refusing: Mutex<bool>,
    /// Each settlement notice it was given, acknowledged or not, in order.
    settlements: Mutex<Vec<Settled>>,
    /// The owner's item whose settlement notice fails, as an unreachable store's would.
    pub(super) unsettling: Mutex<Option<String>>,
    /// Each withdrawal it was asked for, in order.
    withdrawals: Mutex<Vec<Withdrawal>>,
    /// How each next withdrawal is answered -- `None` never -- and once these
    /// run out, withdrawn.
    pub(super) verdicts: Mutex<VecDeque<Option<WithdrawAnswer>>>,
    /// How long each withdrawal waits before it is answered.
    pub(super) withdraw_after: Mutex<Duration>,
    /// How many of its calls are running, and the most that ever ran at once.
    running: AtomicUsize,
    most: AtomicUsize,
    /// Each item it stored, by OutRig's id, with its reference: its durable
    /// record, which a durable backend resolves from.
    kept: Mutex<BTreeMap<MessageId, String>>,
    /// Whether it resolves from its record; otherwise it cannot tell.
    durable: bool,
    /// How each next send's resolution is answered, before its record is read.
    resolutions: Mutex<VecDeque<SendAnswer>>,
    /// What each next send's resolution waits for before it answers -- `None`
    /// nothing -- and once these run out, nothing.
    pub(super) held: Mutex<VecDeque<Option<Arc<tokio::sync::Notify>>>>,
    /// How each next withdrawal's resolution is answered -- `None` with an
    /// error -- and once these run out, withdrawn.
    pub(super) withdrawal_resolutions: Mutex<VecDeque<Option<WithdrawAnswer>>>,
    /// Each resolution it was asked for, of which operation, and each
    /// hand-over of positions.
    queries: Mutex<Vec<(ChannelOperation, Query)>>,
    positions: Mutex<Vec<Positions>>,
}

/// One call of the backend's, running until it answers or is dropped.
struct Running<'a>(&'a Backend);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.running.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Backend {
    pub(super) fn answering(answers: impl IntoIterator<Item = Answer>) -> Arc<Self> {
        let backend = Self::default();
        backend.answers.lock().unwrap().extend(answers);
        Arc::new(backend)
    }

    /// One answering as `answers` says that resolves what it is asked from its record of what
    /// it stored, once `resolutions` run out.
    pub(super) fn durable(
        answers: impl IntoIterator<Item = Answer>,
        resolutions: impl IntoIterator<Item = SendAnswer>,
    ) -> Arc<Self> {
        let backend = Self {
            durable: true,
            ..Self::default()
        };
        backend.answers.lock().unwrap().extend(answers);
        backend.resolutions.lock().unwrap().extend(resolutions);
        Arc::new(backend)
    }

    /// What it stored, by OutRig's id.
    pub(super) fn kept(&self) -> Vec<MessageId> {
        self.kept.lock().unwrap().keys().copied().collect()
    }

    pub(super) fn queries(&self) -> Vec<(ChannelOperation, Query)> {
        self.queries.lock().unwrap().clone()
    }

    pub(super) fn positions(&self) -> Vec<Positions> {
        self.positions.lock().unwrap().clone()
    }

    pub(super) fn sent(&self) -> Vec<Outbound> {
        self.sent.lock().unwrap().clone()
    }

    pub(super) fn settlements(&self) -> Vec<Settled> {
        self.settlements.lock().unwrap().clone()
    }

    pub(super) fn withdrawals(&self) -> Vec<Withdrawal> {
        self.withdrawals.lock().unwrap().clone()
    }

    /// The most of its calls -- of every kind -- that ever ran at once.
    pub(super) fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    /// A call begins.
    fn run(&self) -> Running<'_> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        Running(self)
    }

    /// Each notice's channel and item id, in the order the backend was told.
    pub(super) fn notices(&self) -> Vec<(String, String)> {
        self.notices
            .lock()
            .unwrap()
            .iter()
            .map(|notice| (notice.channel.clone(), notice.item.clone()))
            .collect()
    }
}

impl ChannelBackend for Backend {
    fn send(&self, item: Outbound) -> BackendFuture<'_, SendAnswer> {
        let running = self.run();
        let answer = self
            .answers
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Answer::Accept);
        if matches!(
            answer,
            Answer::Accept | Answer::After(_) | Answer::UncertainKept(_) | Answer::NeverKept
        ) {
            self.kept
                .lock()
                .unwrap()
                .insert(item.item, format!("ref-{}", item.item));
        }
        self.sent.lock().unwrap().push(item);
        Box::pin(async move {
            let _running = running;
            match answer {
                Answer::Accept => SendAnswer::Accepted {
                    reference: Some("ref".to_string()),
                },
                Answer::After(go) => {
                    go.notified().await;
                    SendAnswer::Accepted {
                        reference: Some("ref".to_string()),
                    }
                }
                Answer::NotAccept(reason) => SendAnswer::NotAccepted {
                    reason: reason.to_string(),
                },
                Answer::Uncertain(reason) | Answer::UncertainKept(reason) => {
                    SendAnswer::Uncertain {
                        reason: reason.to_string(),
                    }
                }
                Answer::Never | Answer::NeverKept => std::future::pending().await,
            }
        })
    }

    fn resolve_send(&self, query: Query) -> BackendFuture<'_, SendAnswer> {
        let running = self.run();
        let scripted = self.resolutions.lock().unwrap().pop_front();
        let held = self.held.lock().unwrap().pop_front().flatten();
        let kept = self.kept.lock().unwrap().get(&query.item).cloned();
        let durable = self.durable;
        self.queries
            .lock()
            .unwrap()
            .push((ChannelOperation::Send, query));
        Box::pin(async move {
            let _running = running;
            if let Some(held) = held {
                held.notified().await;
            }
            match (scripted, kept) {
                (Some(scripted), _) => scripted,
                (None, _) if !durable => SendAnswer::Uncertain {
                    reason: "it keeps no record".to_string(),
                },
                (None, Some(reference)) => SendAnswer::Accepted {
                    reference: Some(reference),
                },
                (None, None) => SendAnswer::NotAccepted {
                    reason: "it never stored the item".to_string(),
                },
            }
        })
    }

    fn resolve_withdrawal(&self, query: Query) -> BackendFuture<'_, io::Result<WithdrawAnswer>> {
        let running = self.run();
        let scripted = self
            .withdrawal_resolutions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Some(WithdrawAnswer::Withdrawn));
        self.queries
            .lock()
            .unwrap()
            .push((ChannelOperation::Withdrawal, query));
        Box::pin(async move {
            let _running = running;
            scripted.ok_or_else(|| io::Error::other("the store is unreachable"))
        })
    }

    fn positions(&self, positions: Positions) -> BackendFuture<'_, io::Result<()>> {
        let running = self.run();
        self.positions.lock().unwrap().push(positions);
        Box::pin(async move {
            let _running = running;
            Ok(())
        })
    }

    fn consumed(&self, notice: Consumed) -> BackendFuture<'_, io::Result<()>> {
        let running = self.run();
        self.notices.lock().unwrap().push(notice);
        let refusing = *self.refusing.lock().unwrap();
        Box::pin(async move {
            let _running = running;
            if refusing {
                Err(io::Error::other("the store is unreachable"))
            } else {
                Ok(())
            }
        })
    }

    fn settled(&self, notice: Settled) -> BackendFuture<'_, io::Result<()>> {
        let running = self.run();
        let refusing = notice.item.is_some() && *self.unsettling.lock().unwrap() == notice.item;
        self.settlements.lock().unwrap().push(notice);
        Box::pin(async move {
            let _running = running;
            if refusing {
                Err(io::Error::other("the store is unreachable"))
            } else {
                Ok(())
            }
        })
    }

    fn withdrawn(&self, withdrawal: Withdrawal) -> BackendFuture<'_, io::Result<WithdrawAnswer>> {
        let running = self.run();
        self.withdrawals.lock().unwrap().push(withdrawal);
        let verdict = self
            .verdicts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Some(WithdrawAnswer::Withdrawn));
        let after = *self.withdraw_after.lock().unwrap();
        Box::pin(async move {
            let _running = running;
            tokio::time::sleep(after).await;
            match verdict {
                Some(verdict) => Ok(verdict),
                None => std::future::pending().await,
            }
        })
    }
}

/// `inbox`: text both ways, waking its holder.
pub(super) fn inbox() -> ChannelDefinition {
    ChannelDefinition::new("inbox").description("Messages from the task's user.")
}

/// A session with no primary agent, holding the application, its model calls answered by
/// `routes`, and built further by `build`.
pub(super) async fn holding(
    var: &str,
    routes: Vec<(&'static str, Responder)>,
    build: impl FnOnce(SessionBuilder) -> SessionBuilder,
) -> (Tree, tempfile::TempDir) {
    let (dir, spec) = application();
    let mock = mock_http::routed(routes, None).await;
    let tree = tree_watched(var, "", mock, Watch::Gathered, move |builder| {
        build(builder.no_primary().application(spec))
    })
    .await;
    (tree, dir)
}

/// An `Inbox` holding the channels `channels` name, each under its own name.
pub(super) fn holder(channels: &[&str]) -> ConstructSpec {
    let facilities: BTreeMap<String, Facility> = channels
        .iter()
        .map(|name| ((*name).to_string(), Facility::channel(*name)))
        .collect();
    ConstructSpec::new("app.agents:Inbox", "fast")
        .arg("facilities", Argument::Facilities(facilities))
}

/// Construct `spec` in `tree`'s session and wait until its orienting round has ended.
pub(super) async fn ready(tree: &Tree, spec: ConstructSpec) -> RootHandle {
    let root = within(tree.session.construct(spec))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    root
}

/// Construct `spec` in `tree`'s session, its orienting round left to run: for a test whose code
/// runs there and waits on the test.
pub(super) async fn constructed(tree: &Tree, spec: ConstructSpec) -> RootHandle {
    within(tree.session.construct(spec))
        .await
        .expect("constructed")
}

/// The opening of each round the agent whose class carries `marker` had past its orienting
/// round: each round's first call.
pub(super) fn openings(requests: &[RecordedRequest], marker: &str) -> Vec<String> {
    past_launch(requests, marker)
        .into_iter()
        .filter(|request| !after_result(request))
        .map(opening)
        .collect()
}

/// A responder for an agent's rounds past its orienting round: each round's first call runs
/// the next of `sources` -- the last again once they run out -- and the call after its result
/// ends the round.
pub(super) fn running(sources: &[&str]) -> Responder {
    ready_then(from_launch(sources))
}

/// [`running`], its orienting round among the rounds: the round an item already waiting when
/// the holder launched is announced in, and so the one that reads it.
pub(super) fn from_launch(sources: &[&str]) -> Responder {
    serving_each(sources.iter().map(|source| (*source).to_string()).collect())
}

/// A pushed item is queued, answered with its delivery id, and opens a round for the idle root
/// whose opening says what waits by channel and count and nothing of what it says; its code
/// receives the item with its body, sender and id, and only then does the backend hear it was
/// consumed. Every step is evented under the ids.
#[tokio::test]
async fn a_pushed_item_becomes_a_pending_delivery() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_PUSH_KEY",
        vec![(
            "ROLE inbox",
            running(&["d = await runtime.channels['inbox'].receive()\n\
                 print(d.body, d.sender, d.id, d.correlation)"]),
        )],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    assert_eq!(channel.holder().as_deref(), root.agent());
    assert!(backend.notices().is_empty());
    let item = Inbound::message("m-1", "alice", json!("hello"))
        .correlation("m-0")
        .reference("row-17");
    let delivered = within(channel.deliver(item)).await.expect("queued");
    let Delivered::Queued {
        delivery: DeliveryId::Message(delivery),
    } = delivered
    else {
        panic!("{delivered:?}");
    };
    until("the notice", || !backend.notices().is_empty()).await;
    assert_eq!(
        backend.notices(),
        [("inbox".to_string(), "m-1".to_string())]
    );
    // The backend's reference comes back with the notice, never having been read.
    let notice = backend.notices.lock().unwrap()[0].clone();
    assert_eq!(notice.reference.as_deref(), Some("row-17"));
    assert_eq!(notice.delivery, delivery);
    let agent = root.agent().expect("launched").to_string();
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        openings(&requests, "ROLE inbox")[0],
        "[outrig] 1 message is waiting on runtime.channels[\"inbox\"]."
    );
    let subject = format!("agent/{agent}");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!("hello alice {} m-0\n", delivery.get())]
    );
    let sent = data_of(&events, "message.sent", &subject);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["message"], json!(delivery));
    assert_eq!(sent[0]["from"], "backend");
    assert_eq!(sent[0]["sender"], "alice");
    let received = data_of(&events, "message.received", &subject);
    assert_eq!(received[0]["message"], json!(delivery));
    let consumed = data_of(&events, "channel.consumed", &subject);
    assert_eq!(consumed[0]["item"], "m-1");
    assert_eq!(consumed[0]["delivery"], json!(delivery));
    assert_eq!(
        data_of(&events, "channel.attached", &subject)[0]["wake"],
        "wake"
    );
}

/// The holder's code waiting on `runtime.wait` while an item waits gets `MessageAvailable`
/// naming the channel, and the item is still there: waiting takes nothing, queued is not
/// consumed, and the backend hears nothing until the code receives it.
#[tokio::test]
async fn waiting_leaves_an_item_unread_and_queued_is_not_consumed() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (flag, reached) = (Flag::new(), Flag::new());
    let code = format!(
        "ch = runtime.channels['inbox']\nprint(ch.pending())\n\
         try:\n    await runtime.wait({{asyncio.create_task(asyncio.sleep(5))}})\n\
         except runtime.MessageAvailable as e:\n    print(e.channels, ch.pending())\n\
         open({}, 'w').close()\n{}\nd = await ch.receive()\nprint(ch.pending())",
        reached.py(),
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_WAIT_KEY",
        vec![("ROLE inbox", running(&[&code]))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    within(channel.deliver(Inbound::message("m-1", "alice", json!("hi"))))
        .await
        .expect("queued");
    let subject = format!("agent/{}", agent_of(&root));
    // The code has waited and been told; nothing has been received.
    raised(&reached).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(backend.notices().is_empty(), "queued is not consumed");
    flag.raise();
    until("the notice", || !backend.notices().is_empty()).await;
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(exec_outputs(&events, &subject), ["1\n('inbox',) 1\n0\n"]);
}

/// `runtime.wait` over a child's handle and an external channel is one wait: an arrival while
/// the child works raises `MessageAvailable` with the handle still pending, and the child's
/// reply then returns it in `done`, raising nothing.
#[tokio::test]
async fn one_wait_watches_a_childs_handle_and_a_channel() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let release = Arc::new(tokio::sync::Notify::new());
    let held = Arc::clone(&release);
    let helper: Responder = ready_then(Arc::new(move |request: &RecordedRequest| {
        let release = Arc::clone(&held);
        let canned = if after_result(request) {
            text_reply("ended")
        } else {
            submit(
                "h1",
                &py("d = await runtime.channels['ping'].receive()\nawait d.reply('pong')"),
            )
        };
        let first = !after_result(request);
        Box::pin(async move {
            if first {
                release.notified().await;
            }
            canned
        })
    }));
    let code = "from app.agents import Helper\n\
                helper = Helper()\nawait helper.ready()\nh = helper.ping('x')\n\
                try:\n    await runtime.wait({h})\n\
                except runtime.MessageAvailable as e:\n    print('message', e.channels, h.done())\n\
                d = await runtime.channels['inbox'].receive()\n\
                done, pending = await runtime.wait({h})\n\
                print(h in done, len(pending), await h)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_ONE_WAIT_KEY",
        vec![
            ("ROLE helper", helper),
            ("ROLE inbox", from_launch(&[code])),
        ],
        |builder| builder.channel(inbox(), backend),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    // The helper's request round has begun: the root sent its ping, and waits.
    tree.seen("agent.request.sent", 1).await;
    within(channel.deliver(Inbound::message("m-1", "alice", json!("mid-wait"))))
        .await
        .expect("queued");
    tokio::time::sleep(Duration::from_millis(300)).await;
    release.notify_one();
    let subject = format!("agent/{}", agent_of(&root));
    tree.seen("exec.completed", 2).await;
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let outputs = exec_outputs(&events, &subject);
    assert!(
        outputs
            .iter()
            .any(|output| output == "message ('inbox',) False\nTrue 0 pong\n"),
        "{outputs:?}"
    );
}

/// Pushing an item id the channel has taken already, before and after its holder received
/// it, queues nothing and answers with the first delivery id and where the item is: the holder
/// sees one delivery, and the backend hears of one.
#[tokio::test]
async fn a_repeat_is_answered_and_queues_nothing() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let flag = Flag::new();
    let code = format!(
        "ch = runtime.channels['inbox']\n{}\nd = await ch.receive()\nprint(d.body, ch.pending())",
        flag.awaited()
    );
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_REPEAT_KEY",
        vec![("ROLE inbox", running(&[&code]))],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    let item = || Inbound::message("m-1", "alice", json!("once"));
    let first = within(channel.deliver(item())).await.expect("queued");
    let again = within(channel.deliver(item())).await.expect("a repeat");
    assert_eq!(
        again,
        Delivered::Repeat {
            delivery: first.delivery(),
            position: Position::Unread,
        }
    );
    flag.raise();
    until("the notice", || !backend.notices().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = within(channel.deliver(item())).await.expect("a repeat");
    assert_eq!(after.delivery(), first.delivery());
    assert!(
        matches!(
            after,
            Delivered::Repeat {
                position: Position::Acknowledged | Position::Consumed,
                ..
            }
        ),
        "{after:?}"
    );
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(exec_outputs(&events, &subject), ["once 0\n"]);
    assert_eq!(backend.notices().len(), 1);
    assert_eq!(data_of(&events, "message.sent", &subject).len(), 1);
}

/// A send reaches the backend with the item's id and returns that id once it is accepted; one
/// the backend did not take raises saying a retry is safe; one it cannot vouch for, or that runs
/// past its bound, raises with the item's id once the backend, asked, cannot tell either; one
/// outside the contract or past the size limit raises before anything reaches the backend. Each
/// outcome is evented, and the round's facts list what it sent.
#[tokio::test]
async fn a_send_uses_the_backend_and_its_answers_are_told_apart() {
    let backend = Backend::answering([
        Answer::Accept,
        Answer::NotAccept("no room in the thread"),
        Answer::Uncertain("the store timed out"),
        Answer::Never,
    ]);
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\nd = await ch.receive()\n\
                print(await ch.send('hi'))\n\
                try:\n    await ch.send('no')\nexcept outrig.SendNotAccepted as e:\n    print('not', 'safe' in str(e), e.reason)\n\
                try:\n    await ch.send('maybe')\nexcept outrig.SendUncertain as e:\n    print('uncertain', type(e.item).__name__, e.reason.split(';')[0])\n\
                try:\n    await ch.send('never')\nexcept outrig.SendUncertain as e:\n    print('late', 'within' in e.reason)\n\
                try:\n    await ch.send(1)\nexcept TypeError as e:\n    print('type')\n\
                try:\n    await ch.send('x' * (1 << 20))\nexcept ValueError as e:\n    print('value')";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_SEND_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder.channel(
                inbox()
                    .call_timeout(Duration::from_millis(300))
                    .deliver(Inbound::message("m-0", "alice", json!("go"))),
                given,
            )
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let sent = backend.sent();
    let bodies: Vec<&Value> = sent.iter().map(|item| &item.body).collect();
    assert_eq!(
        bodies,
        [&json!("hi"), &json!("no"), &json!("maybe"), &json!("never")]
    );
    assert_eq!(sent[0].channel, "inbox");
    assert_eq!(sent[0].body_type, "str");
    assert_eq!(sent[0].sender, root.agent().expect("launched"));
    let rounds = within(async {
        loop {
            let rounds = root.rounds();
            if rounds.iter().any(|facts| facts.any_send()) {
                break rounds;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let facts = rounds
        .iter()
        .find(|facts| facts.any_send())
        .expect("a round sent");
    assert_eq!(facts.sends.len(), 1);
    assert_eq!(facts.sends[0].channel, "inbox");
    let items: Vec<_> = sent.iter().map(|item| item.item).collect();
    assert_eq!(facts.sends[0].items, items);
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        [format!(
            "{}\nnot True no room in the thread\nuncertain int the store timed out\nlate True\n\
             type\nvalue\n",
            items[0].get()
        )]
    );
    let outcomes: Vec<Value> = data_of(&events, "channel.item.outcome", &subject)
        .into_iter()
        .map(|data| data["outcome"].clone())
        .collect();
    assert_eq!(
        outcomes,
        [
            json!("accepted"),
            json!("not_accepted"),
            json!("uncertain"),
            json!("uncertain")
        ]
    );
    assert_eq!(
        data_of(&events, "channel.item.outcome", &subject)[0]["reference"],
        "ref"
    );
    // The initial item was pushed before any agent held the channel, so its `message.sent` is
    // the session's.
    assert_eq!(
        data_of(&events, "message.sent", &subject).len(),
        4,
        "four sends"
    );
}

/// Two channels sharing a backend have separate call slots and timeouts. A slow
/// store can keep eight calls running beyond another channel's shorter bound.
#[tokio::test]
async fn backend_call_bounds_belong_to_each_channel() {
    let go = Arc::new(tokio::sync::Notify::new());
    let backend = Backend::answering(std::iter::repeat_n(Answer::After(go.clone()), 9));
    let slow: Arc<dyn ChannelBackend> = backend.clone();
    let fast: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\n\
                slow = [asyncio.create_task(runtime.channels['slow'].send(str(i))) for i in range(8)]\n\
                try:\n    await runtime.channels['fast'].send('fast')\nexcept outrig.SendUncertain:\n    print('fast uncertain')\n\
                await asyncio.gather(*slow)\nprint('slow accepted')";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_BOUNDS_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder
                .channel(
                    ChannelDefinition::new("slow").call_timeout(Duration::from_secs(60)),
                    slow,
                )
                .channel(
                    ChannelDefinition::new("fast").call_timeout(Duration::from_secs(2)),
                    fast,
                )
        },
    )
    .await;
    let root = constructed(&tree, holder(&["slow", "fast"])).await;
    until("nine calls sharing a backend", || backend.sent().len() == 9).await;
    tree.seen("channel.item.outcome", 1).await;
    assert_eq!(backend.most(), 9, "each channel has its own call slots");
    assert_eq!(
        backend.running.load(Ordering::SeqCst),
        8,
        "slow calls still running"
    );
    go.notify_waiters();
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(
        exec_outputs(&events, &subject),
        ["fast uncertain\nslow accepted\n"]
    );
    let outcomes = data_of(&events, "channel.item.outcome", &subject);
    assert_eq!(outcomes.len(), 9);
    assert_eq!(outcomes[0]["channel"], "fast");
    assert_eq!(outcomes[0]["outcome"], "uncertain");
    for outcome in &outcomes[1..] {
        assert_eq!(outcome["channel"], "slow");
        assert_eq!(outcome["outcome"], "accepted");
    }
}

/// With 16 sends unanswered, the 17th waits until one is answered.
#[tokio::test]
async fn the_seventeenth_unanswered_send_waits() {
    let backend = Backend::answering(std::iter::repeat_n(Answer::Never, 16));
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\nd = await ch.receive()\n\
                sends = [asyncio.create_task(ch.send(f'm{i}')) for i in range(17)]\n\
                ended = await asyncio.gather(*sends, return_exceptions=True)\n\
                print(sorted(type(e).__name__ for e in ended))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_WINDOW_SEND_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder.channel(
                inbox()
                    .call_timeout(Duration::from_secs(2))
                    .deliver(Inbound::message("m-0", "alice", json!("go"))),
                given,
            )
        },
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    // Each send the host is given is evented as it arrives: 16 before any is answered, the
    // backend answering none of them within its two seconds.
    let sends = || {
        let events = tree.events.lock().expect("events");
        kind(&events, "message.sent", Some(&subject)).len()
    };
    until("sixteen sends", || sends() == 16).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(sends(), 16, "the seventeenth waits");
    until("the seventeenth", || sends() == 17).await;
    tree.seen("exec.completed", 1).await;
    let (report, events, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let outputs = exec_outputs(&events, &subject);
    // The seventeenth was taken, and its send returned the id it was given.
    assert!(outputs[0].contains("'int'"), "{outputs:?}");
    assert_eq!(
        outputs[0].matches("SendUncertain").count(),
        16,
        "{outputs:?}"
    );
}

/// The backend is no binding, no global and nothing of the endpoint's: in the holder's kernel
/// nothing names it.
#[tokio::test]
async fn the_agent_cannot_reach_the_backend() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    // What the endpoint holds is the interpreter's own -- its lock, its queue, its contract's
    // schemas, the kernel holding it -- and nothing of any other module's.
    let code = "ch = runtime.channels['inbox']\nd = await ch.receive()\n\
                print(dict(runtime.facilities), list(runtime.names()))\n\
                print([name for name in dir(ch) if 'backend' in name.lower()])\n\
                own = ('builtins', '_thread', 'collections', 'outrig.schema', '__main__')\n\
                print(sorted({type(getattr(ch, name)).__module__ for name in dir(ch)\n\
                              if not name.startswith('__')} - set(own)))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_UNREACHABLE_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder.channel(
                inbox().deliver(Inbound::message("m-0", "alice", json!("go"))),
                backend,
            )
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    let outputs = exec_outputs(&events, &subject);
    assert_eq!(outputs, ["{} ['ch', 'd']\n[]\n[]\n"], "{outputs:?}");
}

/// The window: with a bound of 2 and two unread, a push is refused `full` and `room()` waits;
/// once the holder receives one, it resolves and a push is taken. An item outside the contract
/// is refused naming the problem, and nothing is queued. A push before any kernel holds the
/// channel waits for the one that takes it, and `holder()` reads none until then; a channel no
/// dict names is listed at the close with its items unread.
#[tokio::test]
async fn the_window_is_bounded_and_held_until_a_holder_takes_it() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let feed: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_WINDOW_KEY",
        vec![(
            "ROLE inbox",
            from_launch(&["d = await runtime.channels['inbox'].receive()\nprint(d.body)"]),
        )],
        |builder| {
            builder
                .channel(inbox().bound(2), backend)
                .channel(ChannelDefinition::new("feed"), feed)
        },
    )
    .await;
    let channel = tree.session.channel("inbox").expect("declared");
    assert_eq!(channel.holder(), None);
    for (item, body) in [("m-1", "first"), ("m-2", "second")] {
        within(channel.deliver(Inbound::message(item, "alice", json!(body))))
            .await
            .expect("queued for whoever holds it");
    }
    assert_eq!(
        within(channel.deliver(Inbound::message("m-3", "alice", json!("third")))).await,
        Err(DeliverError::Full)
    );
    assert_eq!(
        within(channel.deliver(Inbound::message("m-4", "alice", json!(4)))).await,
        Err(DeliverError::Contract(vec![
            "$: expected a string, not an integer".to_string()
        ]))
    );
    let room = tokio::spawn(channel.room());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!room.is_finished(), "no room while two wait");
    let feed = tree.session.channel("feed").expect("declared");
    within(feed.deliver(Inbound::message("f-1", "bob", json!("unheard"))))
        .await
        .expect("queued");
    let root = ready(&tree, holder(&["inbox"])).await;
    assert_eq!(channel.holder().as_deref(), root.agent());
    within(room).await.expect("room");
    within(channel.deliver(Inbound::message("m-3", "alice", json!("third"))))
        .await
        .expect("queued now");
    let subject = format!("agent/{}", agent_of(&root));
    tree.seen("exec.completed", 1).await;
    let (report, events, requests) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject)[0], "first\n");
    // Its orienting round named what waited.
    let orienting = of_agent(&requests, "ROLE inbox")
        .into_iter()
        .find(|request| launching(request))
        .map(opening)
        .expect("an orienting round");
    assert!(
        orienting.contains("2 messages are waiting on runtime.channels[\"inbox\"]"),
        "{orienting}"
    );
    let feed = report
        .channels
        .iter()
        .find(|channel| channel.channel == "feed")
        .expect("feed is reported");
    assert_eq!(feed.holder, None);
    assert_eq!(feed.unread, ["f-1"]);
}

/// A definition's initial items wait in the window from the start: the root a builder
/// constructs as the session starts, and one the session constructs later, each open their
/// orienting round with their instructions whole and the opening naming what waits, and receive
/// it as any later item. A push through the handle before `Session::construct` does the same.
#[tokio::test]
async fn initial_input_is_a_delivery_like_any_other() {
    let waits = "ch = runtime.channels['inbox']\n\
                 try:\n    await runtime.wait({asyncio.create_task(asyncio.sleep(5))})\n\
                 except runtime.MessageAvailable:\n    d = await ch.receive()\n    print(d.body)";
    let orienting = |code: &'static str| -> Responder {
        Arc::new(move |request: &RecordedRequest| {
            let canned = if after_result(request) {
                text_reply("ended")
            } else {
                submit("o1", &py(code))
            };
            Box::pin(async move { canned })
        })
    };
    let question = || Inbound::message("m-0", "alice", json!("What does this project do?"));
    for form in ["builder", "construct", "pushed"] {
        let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
        let definition = match form {
            "pushed" => inbox(),
            _ => inbox().deliver(question()),
        };
        let builder_form = form == "builder";
        let (tree, _app) = holding(
            "OUTRIG_TEST_CHANNEL_INITIAL_KEY",
            vec![("ROLE inbox", orienting(waits))],
            move |builder| {
                let builder = builder.channel(definition, backend);
                if builder_form {
                    builder.construct(holder(&["inbox"]))
                } else {
                    builder
                }
            },
        )
        .await;
        if form == "pushed" {
            let channel = tree.session.channel("inbox").expect("declared");
            within(channel.deliver(question())).await.expect("queued");
        }
        let root = match form {
            "builder" => tree.session.constructed().expect("constructed at start"),
            _ => within(tree.session.construct(holder(&["inbox"])))
                .await
                .expect("constructed"),
        };
        within(root.ready()).await.expect("ready");
        let subject = format!("agent/{}", agent_of(&root));
        let (report, events, requests) = tree.end().await;
        assert_eq!(report.verdict(), Verdict::Clean, "{form}: {report:?}");
        assert_eq!(
            exec_outputs(&events, &subject),
            ["What does this project do?\n"],
            "{form}"
        );
        let first = of_agent(&requests, "ROLE inbox")[0];
        assert!(
            opening(first).ends_with("1 message is waiting on runtime.channels[\"inbox\"]."),
            "{form}: {}",
            opening(first)
        );
        let instructions = system_text(&first.body);
        assert!(
            !instructions.contains("What does this project do?"),
            "{form}: no item's body is in the instructions"
        );
    }
}

/// A root built with no initial input runs its orienting round on its lead alone: nothing is
/// announced or invented.
#[tokio::test]
async fn empty_initial_input_announces_nothing() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_EMPTY_KEY",
        vec![("ROLE inbox", running(&["pass"]))],
        |builder| builder.channel(inbox(), backend),
    )
    .await;
    ready(&tree, holder(&["inbox"])).await;
    let (_, _, requests) = tree.end().await;
    let first = opening(of_agent(&requests, "ROLE inbox")[0]);
    assert_eq!(
        first,
        format!("[outrig] {}.", super::orientation::orienting(false, ""))
    );
}

/// An item may name a skill: it reaches the holder with `skill` set, and
/// `skill.directive.received` names the channel. One naming a skill the session lacks is refused,
/// and a request on a plain channel is refused `contract`; neither queues anything.
#[tokio::test]
async fn a_directive_crosses_as_an_item() {
    let skills = tempfile::tempdir().expect("a skills directory");
    std::fs::create_dir(skills.path().join("review")).expect("a skill");
    std::fs::write(
        skills.path().join("review/SKILL.md"),
        "---\nname: review\ndescription: Reviews a change.\n---\nSteps.\n",
    )
    .expect("its instructions");
    let source = skills.path().to_path_buf();
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_DIRECTIVE_KEY",
        vec![(
            "ROLE inbox",
            running(&["d = await runtime.channels['inbox'].receive()\nprint(d.skill, d.body)"]),
        )],
        move |builder| {
            builder
                .skill_source("test", DirectorySource::new(source))
                .channel(inbox(), backend)
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    assert_eq!(
        within(channel.deliver(Inbound::message("m-0", "alice", json!("x")).skill("nope"))).await,
        Err(DeliverError::UnknownSkill("nope".to_string()))
    );
    assert!(matches!(
        within(
            channel.deliver(
                Inbound::message("m-1", "alice", json!("x"))
                    .kind(ItemKind::Request)
                    .skill("review")
            )
        )
        .await,
        Err(DeliverError::Contract(_))
    ));
    within(channel.deliver(Inbound::message("m-2", "alice", json!("the diff")).skill("review")))
        .await
        .expect("queued");
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject), ["review the diff\n"]);
    let directive = data_of(&events, "skill.directive.received", &subject);
    assert_eq!(directive.len(), 1);
    assert_eq!(directive[0]["channel"], "inbox");
    assert_eq!(directive[0]["skill"], "review");
    let instructions = system_text(&of_agent(&requests, "ROLE inbox")[0].body);
    assert!(instructions.contains("`d.skill`"), "{instructions}");
}

/// A body the definition types is checked as JSON on arrival -- one missing a field is refused
/// naming it, nothing queued -- and built into the declared class as the holder receives it, on
/// its own loop: never on the thread that read it.
#[tokio::test]
async fn a_typed_body_is_checked_on_arrival_and_built_at_receive() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let code = "import app.messages\nd = await runtime.channels['notes'].receive()\n\
                print(type(d.body).__name__, d.body.text, d.body.level)\n\
                print([name.startswith('kernel-') for name in app.messages.BUILT])";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_TYPED_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder.channel(
                ChannelDefinition::new("notes")
                    .receives(Contract::Typed("app.messages:Note".to_string()))
                    .sends(Contract::Json),
                backend,
            )
        },
    )
    .await;
    let channel = tree.session.channel("notes").expect("declared");
    assert_eq!(
        within(channel.deliver(Inbound::message("n-0", "alice", json!({"text": "hi"})))).await,
        Err(DeliverError::Contract(vec![
            "$.level: missing required key".to_string()
        ]))
    );
    within(channel.deliver(Inbound::message(
        "n-1",
        "alice",
        json!({"text": "hi", "level": 2}),
    )))
    .await
    .expect("queued");
    let root = ready(&tree, holder(&["notes"])).await;
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject), ["Note hi 2\n[True]\n"]);
    let instructions = system_text(&of_agent(&requests, "ROLE inbox")[0].body);
    assert!(
        instructions.contains("a `Note`, the class `app.messages` holds"),
        "{instructions}"
    );
    assert!(
        instructions.contains("\"level\""),
        "the schema: {instructions}"
    );
}

/// On an `announce` channel an arrival during a wait does not end it; the next result's prefix
/// announces it, and the round it was announced in is the one it earns. Arriving at an idle
/// holder, it opens a round. On a `silent` channel nothing is announced and no round opens,
/// though `pending()` counts it.
#[tokio::test]
async fn the_wake_policy_decides_what_an_arrival_does() {
    let inboxed = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = inboxed.clone();
    let news: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let quiet: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let code = "t = asyncio.create_task(asyncio.sleep(1.5))\n\
                d = await runtime.channels['inbox'].receive()\n\
                done, pending = await runtime.wait({t})\n\
                print(len(done), runtime.channels['news'].pending(), runtime.channels['quiet'].pending())";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_WAKE_KEY",
        vec![("ROLE inbox", running(&[code, "pass"]))],
        |builder| {
            builder
                .channel(inbox(), given)
                .channel(ChannelDefinition::new("news").wake(Wake::Announce), news)
                .channel(ChannelDefinition::new("quiet").wake(Wake::Silent), quiet)
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox", "news", "quiet"])).await;
    let (inbox_end, news_end, quiet_end) = (
        tree.session.channel("inbox").expect("declared"),
        tree.session.channel("news").expect("declared"),
        tree.session.channel("quiet").expect("declared"),
    );
    within(quiet_end.deliver(Inbound::message("q-1", "feed", json!("shh"))))
        .await
        .expect("queued");
    within(inbox_end.deliver(Inbound::message("i-1", "alice", json!("go"))))
        .await
        .expect("queued");
    until("the inbox item taken", || !inboxed.notices().is_empty()).await;
    within(news_end.deliver(Inbound::message("n-1", "feed", json!("later"))))
        .await
        .expect("queued");
    tree.seen("exec.completed", 1).await;
    // The round's end is the news item's: it was announced there.
    tokio::time::sleep(Duration::from_millis(500)).await;
    within(news_end.deliver(Inbound::message("n-2", "feed", json!("again"))))
        .await
        .expect("queued");
    tree.seen("exec.completed", 2).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject)[0], "1 1 1\n");
    let calls = past_launch(&requests, "ROLE inbox");
    let announced = calls
        .iter()
        .find(|request| after_result(request))
        .map(|request| super::agent_tests::tool_result(request, "s0"))
        .expect("the round's result");
    assert!(
        announced.starts_with("[1 message is waiting on runtime.channels[\"news\"]]"),
        "{announced}"
    );
    let opened = openings(&requests, "ROLE inbox");
    assert_eq!(
        opened,
        [
            "[outrig] 1 message is waiting on runtime.channels[\"inbox\"].".to_string(),
            "[outrig] 2 messages are waiting on runtime.channels[\"news\"].".to_string(),
        ],
        "a round for the inbox item, and one for the second news item; none for quiet"
    );
}

/// One holder: the root built with the channel has it, and its instructions list it; a child it
/// constructs with other facilities has no channel; one constructed with the same reference
/// fails at readiness naming the root.
#[tokio::test]
async fn a_channel_has_one_holder() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let helper = ready_then(Arc::new(|request: &RecordedRequest| {
        let canned = if after_result(request) {
            text_reply("ended")
        } else {
            submit(
                "h1",
                &py(
                    "d = await runtime.channels['ping'].receive()\nawait d.reply(str(sorted(runtime.channels)))",
                ),
            )
        };
        Box::pin(async move { canned })
    }));
    let code = "import outrig\nfrom app.agents import Helper\nd = await runtime.channels['inbox'].receive()\n\
                files = Helper(facilities={'files': object()})\nawait files.ready()\n\
                print(await files.ping('x'))\n\
                print(outrig.facility('inbox').pending(), self.facilities['inbox'].pending())\n\
                second = Helper(facilities={'inbox': self.facilities['inbox']})\n\
                try:\n    await second.ready()\nexcept Exception as e:\n    print(type(e).__name__, 'held by agent' in str(e))\n\
                print(sorted(runtime.channels))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_ONE_HOLDER_KEY",
        vec![
            ("ROLE helper", helper),
            ("ROLE inbox", from_launch(&[code])),
        ],
        |builder| {
            builder.channel(
                inbox().deliver(Inbound::message("m-0", "alice", json!("go"))),
                backend,
            )
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    tree.seen("exec.completed", 2).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    let outputs = exec_outputs(&events, &subject);
    assert_eq!(
        outputs.last().map(String::as_str),
        Some("['ping']\n0 0\nRuntimeError True\n['__questions', 'inbox']\n"),
        "{outputs:?}"
    );
    let root_text = system_text(&of_agent(&requests, "ROLE inbox")[0].body);
    assert!(
        root_text.contains(
            "- `runtime.channels[\"inbox\"]`, a plain channel: Messages from the task's user."
        ),
        "{root_text}"
    );
    let helper_text = system_text(&of_agent(&requests, "ROLE helper")[0].body);
    assert!(!helper_text.contains("inbox"), "{helper_text}");
}

/// A channel reaches a root only in a dict of its facilities: one given as an argument of its
/// own refuses the construction, as does one naming a channel the session does not declare.
#[tokio::test]
async fn a_channel_is_given_only_in_a_facility_dict() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_ARGUMENT_KEY",
        vec![("ROLE inbox", running(&["pass"]))],
        |builder| builder.channel(inbox(), backend),
    )
    .await;
    for spec in [
        ConstructSpec::new("app.agents:Inbox", "fast")
            .arg("facilities", Argument::Facility(Facility::channel("inbox"))),
        holder(&["outbox"]),
    ] {
        let root = within(tree.session.construct(spec)).await.expect("asked");
        let error = within(root.ready()).await.expect_err("refused");
        let said = error.to_string();
        assert!(said.contains("invalid-argument"), "{said}");
    }
    assert_eq!(
        tree.session.channel("inbox").expect("declared").holder(),
        None
    );
    tree.end().await;
}

/// In a session with a primary the harness built, a declared channel no dict names is the
/// primary's: its instructions list it, and an arrival is announced as a round opens.
#[tokio::test]
async fn the_primary_holds_a_channel_no_dict_names() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let mock = mock_http::routed(
        vec![(
            "",
            super::children_tests::primary_runs(
                "d = await runtime.channels['inbox'].receive()\nprint(d.body)",
            ),
        )],
        None,
    )
    .await;
    let mut tree = tree_watched(
        "OUTRIG_TEST_CHANNEL_PRIMARY_KEY",
        "",
        mock,
        Watch::Gathered,
        |builder| builder.channel(inbox(), backend),
    )
    .await;
    let channel = tree.session.channel("inbox").expect("declared");
    assert_eq!(channel.holder().as_deref(), Some("primary"));
    within(channel.deliver(Inbound::message("m-1", "alice", json!("for the primary"))))
        .await
        .expect("queued");
    within(tree.session.round()).await.expect("a round");
    let (_, events, requests) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, "agent/primary"),
        ["for the primary\n"]
    );
    let first = &requests[0];
    assert!(
        opening(first).contains("1 message is waiting on runtime.channels[\"inbox\"]"),
        "{}",
        opening(first)
    );
    assert!(system_text(&first.body).contains("`runtime.channels[\"inbox\"]`, a plain channel"));
}

/// A runner-driven holder: an arrival opens a round naming the channel and count; an item left
/// unread earns no second round; a round that receives everything ends with nothing further
/// opened. Nothing warns about a plain item, and nothing is cancelled.
#[tokio::test]
async fn an_arrival_opens_one_round_for_an_idle_holder() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_RUNNER_KEY",
        vec![(
            "ROLE inbox",
            running(&[
                "print('ignoring')",
                "ch = runtime.channels['inbox']\nprint((await ch.receive()).body, (await ch.receive()).body)",
            ]),
        )],
        |builder| builder.channel(inbox(), backend),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let channel = tree.session.channel("inbox").expect("declared");
    within(channel.deliver(Inbound::message("m-1", "alice", json!("one"))))
        .await
        .expect("queued");
    tree.seen("exec.completed", 1).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    within(channel.deliver(Inbound::message("m-2", "alice", json!("two"))))
        .await
        .expect("queued");
    tree.seen("exec.completed", 2).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (report, events, requests) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    assert_eq!(exec_outputs(&events, &subject), ["ignoring\n", "one two\n"]);
    assert_eq!(
        openings(&requests, "ROLE inbox"),
        [
            "[outrig] 1 message is waiting on runtime.channels[\"inbox\"].".to_string(),
            "[outrig] 2 messages are waiting on runtime.channels[\"inbox\"].".to_string(),
        ]
    );
    assert!(kind(&events, "agent.request.cancelled", None).is_empty());
}

/// The far end's close fails the endpoint: a wait says so once and not again, and `receive` and
/// `send` raise from then on. It is evented.
#[tokio::test]
async fn closing_a_channel_reaches_its_holder_once() {
    let inboxed = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = inboxed.clone();
    let peer: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let code = "import outrig\nd = await runtime.channels['inbox'].receive()\n\
                t = asyncio.create_task(asyncio.sleep(30))\n\
                try:\n    await runtime.wait({t}, timeout=20)\nexcept outrig.ChannelClosed as e:\n    print('wait', e.reason)\n\
                done, pending = await runtime.wait({t}, timeout=0.1)\nprint('again', len(done))\n\
                try:\n    await runtime.channels['peer'].receive()\nexcept outrig.ChannelClosed:\n    print('receive')\n\
                try:\n    await runtime.channels['peer'].send('x')\nexcept outrig.ChannelClosed:\n    print('send')\n\
                _ = t.cancel()";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_CLOSE_KEY",
        vec![("ROLE inbox", running(&[code]))],
        |builder| {
            builder
                .channel(inbox(), given)
                .channel(ChannelDefinition::new("peer"), peer)
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox", "peer"])).await;
    within(
        tree.session
            .channel("inbox")
            .expect("declared")
            .deliver(Inbound::message("m-1", "alice", json!("go"))),
    )
    .await
    .expect("queued");
    until("the item taken", || !inboxed.notices().is_empty()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let peer = tree.session.channel("peer").expect("declared");
    peer.close("peer gone");
    assert_eq!(
        within(peer.deliver(Inbound::message("p-1", "bob", json!("late")))).await,
        Err(DeliverError::Closed("peer gone".to_string()))
    );
    tree.seen("exec.completed", 1).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert_eq!(
        exec_outputs(&events, &subject),
        ["wait peer gone\nagain 0\nreceive\nsend\n"]
    );
    let closed = data_of(&events, "channel.closed", &subject);
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["reason"], "peer gone");
}

/// The close: items unread are left and listed by id, never consumed; a send the backend holds
/// is listed with the end it came to -- `uncertain` once the drain is over -- and a notice the
/// backend never acknowledged is listed too.
#[tokio::test]
async fn the_close_reports_what_a_channel_left() {
    let backend = Backend::answering([Answer::Never]);
    *backend.refusing.lock().unwrap() = true;
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "ch = runtime.channels['inbox']\nd = await ch.receive()\n\
                held = asyncio.create_task(ch.send('held'))";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_CLOSE_ROWS_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder.channel(
                inbox()
                    .call_timeout(Duration::from_secs(60))
                    .deliver(Inbound::message("m-1", "alice", json!("one")))
                    .deliver(Inbound::message("m-2", "alice", json!("two")))
                    .deliver(Inbound::message("m-3", "alice", json!("three"))),
                given,
            )
        },
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    until("the send", || !backend.sent().is_empty()).await;
    until("a notice", || !backend.notices().is_empty()).await;
    let agent = root.agent().expect("launched").to_string();
    let (report, _, _) = tree.end().await;
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    let inbox = &report.channels[0];
    assert_eq!(inbox.channel, "inbox");
    assert_eq!(inbox.holder.as_deref(), Some(agent.as_str()));
    assert_eq!(inbox.unread, ["m-2", "m-3"]);
    assert_eq!(inbox.unacknowledged, ["m-1"]);
    assert_eq!(inbox.sends.len(), 1);
    assert_eq!(inbox.sends[0].outcome, ItemOutcome::Uncertain);
    assert_eq!(inbox.sends[0].agent, agent);
}

/// The holder's instructions name each channel it holds with its description and what an arrival
/// does, say a message owes no reply where a request does, and hold no item's body.
#[tokio::test]
async fn the_manifest_describes_each_channel_and_no_item() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let quiet: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_MANIFEST_KEY",
        vec![("ROLE inbox", running(&["pass"]))],
        |builder| {
            builder
                .channel(
                    inbox().deliver(Inbound::message("m-0", "alice", json!("SECRET BODY"))),
                    backend,
                )
                .channel(
                    ChannelDefinition::new("quiet")
                        .wake(Wake::Silent)
                        .description("A feed you read when you choose.")
                        .sends(Contract::Nothing),
                    quiet,
                )
        },
    )
    .await;
    ready(&tree, holder(&["inbox", "quiet"])).await;
    let (_, _, requests) = tree.end().await;
    let text = system_text(&of_agent(&requests, "ROLE inbox")[0].body);
    for expected in [
        "- `runtime.channels[\"inbox\"]`, a plain channel: Messages from the task's user.",
        "An arrival ends a `runtime.wait`",
        "- `runtime.channels[\"quiet\"]`, a plain channel: A feed you read when you choose.",
        "What you send: nothing",
        "An arrival is not announced and opens no round",
        "a message owes no reply -- unlike a request",
        "then when a message arrives on one of your channels",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert!(!text.contains("SECRET BODY"));
}

/// The builder refuses a channel it cannot declare, before anything starts.
#[test]
fn the_builder_refuses_a_channel_it_cannot_declare() {
    let config = crate::config::Config::load_from_str("").expect("an empty config");
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let check = |definition: ChannelDefinition, second: Option<ChannelDefinition>| {
        let mut builder = SessionBuilder::new(config.clone(), None, None)
            .no_primary()
            .channel(definition, Arc::clone(&backend));
        if let Some(second) = second {
            builder = builder.channel(second, Arc::clone(&backend));
        }
        match builder.check() {
            Ok(()) => "ok".to_string(),
            Err(SessionError::Start(failure)) => failure.to_string(),
            Err(other) => other.to_string(),
        }
    };
    assert_eq!(check(inbox(), None), "ok");
    for (definition, second, refused) in [
        (ChannelDefinition::new("user"), None, "user channel"),
        (
            ChannelDefinition::new("runtime"),
            None,
            "cannot take that name",
        ),
        (
            ChannelDefinition::new("not a name"),
            None,
            "cannot take that name",
        ),
        (inbox(), Some(inbox()), "given through `channel` already"),
        (inbox().bound(0), None, "must be 1 to 256"),
        (inbox().bound(257), None, "must be 1 to 256"),
        (
            inbox().receives(Contract::Typed("app.messages:Note".to_string())),
            None,
            "the session has none",
        ),
        (
            inbox().receives(Contract::Typed("Note".to_string())),
            None,
            "module:Name",
        ),
    ] {
        let said = check(definition, second);
        assert!(said.contains(refused), "{refused}: {said}");
    }
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::path::Path;

    use super::*;
    use crate::config::{Config, MountAccess};
    use crate::python::testing::{ALPINE, pull};
    use crate::{LaunchSpec, MountSpec};

    /// A `ProjectReader` the application holds, which answers what arrives on its channel about
    /// the project under `root`, on that channel.
    const READER: &str = r#"
import outrig


class ProjectReader(outrig.Agent):
    """You read the project under the directory `root` names, and answer questions about it from
    what its files say: start with its README and its manifests. Questions arrive on
    `runtime.channels["inbox"]`; answer each in a few sentences of plain text by sending your
    answer there, with `await runtime.channels["inbox"].send(answer)`."""

    def __init__(self, root, facilities=None):
        super().__init__(facilities=facilities)
        self.root = root
"#;

    /// Against a real model, opted into with `OUTRIG_E2E_REAL_API=1` and
    /// `OUTRIG_E2E_GLOBAL_CONFIG` naming a global configuration whose providers and
    /// `default-model` it runs on, their keys in the environment: a Rust owner constructs the
    /// application's primary holding an `inbox` channel whose initial message asks what this
    /// repository -- mounted read-only -- does; the model's code sends its answer on the
    /// channel, the backend's `send` receives it, and the session's report is clean.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_root_answers_on_its_channel() {
        if std::env::var("OUTRIG_E2E_REAL_API").ok().as_deref() != Some("1") {
            eprintln!(
                "[outrig-e2e] skipping a_root_answers_on_its_channel (set OUTRIG_E2E_REAL_API=1 \
                 and OUTRIG_E2E_GLOBAL_CONFIG to enable)"
            );
            return;
        }
        let global = std::env::var("OUTRIG_E2E_GLOBAL_CONFIG").expect(
            "OUTRIG_E2E_REAL_API=1 set without OUTRIG_E2E_GLOBAL_CONFIG -- name the global \
             configuration that declares the model",
        );
        pull(ALPINE).await;
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("this repository");
        let app = tempfile::tempdir().expect("an application directory");
        std::fs::create_dir_all(app.path().join("reader")).expect("the package");
        std::fs::write(app.path().join("reader/__init__.py"), "").expect("write");
        std::fs::write(app.path().join("reader/agents.py"), READER).expect("write");
        let logs = tempfile::tempdir().expect("a logs dir");
        let config =
            Config::load(app.path(), Some(Path::new(&global))).unwrap_or_else(|e| panic!("{e}"));
        let model = config.default_model.clone().expect("a default-model");
        let backend = Arc::new(Backend::default());
        let given: Arc<dyn ChannelBackend> = backend.clone();
        let session = SessionBuilder::new(config, None, None)
            .no_primary()
            .application(ApplicationSpec::directory(app.path()).imports(["reader.agents"]))
            .container(
                LaunchSpec::from_image(ALPINE, BTreeMap::new(), logs.path().to_path_buf())
                    .with_mount(MountSpec::new(
                        &repository,
                        &repository,
                        MountAccess::ReadOnly,
                    )),
            )
            .channel(
                inbox().deliver(Inbound::message(
                    "q-1",
                    "user",
                    json!("What does this project do?"),
                )),
                given,
            )
            .construct(
                ConstructSpec::new("reader.agents:ProjectReader", model)
                    .arg(
                        "root",
                        Argument::data(json!(repository.to_str().expect("a UTF-8 path"))),
                    )
                    .arg(
                        "facilities",
                        Argument::Facilities(BTreeMap::from([(
                            "inbox".to_string(),
                            Facility::channel("inbox"),
                        )])),
                    ),
            )
            .start()
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let wait = std::time::Duration::from_secs(600);
        tokio::time::timeout(wait, async {
            while backend.sent().is_empty() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("answered in time");
        let answer = backend.sent()[0].body.clone();
        assert!(
            answer.as_str().is_some_and(|text| !text.trim().is_empty()),
            "{answer}"
        );
        assert_eq!(
            backend.notices(),
            [("inbox".to_string(), "q-1".to_string())]
        );
        let report = super::super::agent_tests::shut_down(session).await;
        assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    }
}

/// A holder released with sends its backend has not answered leaves the next holder the whole
/// window: what the first held of it is given back as it goes, and its sends still waiting for
/// their turn at the backend are refused rather than sent for no one to hear.
#[tokio::test]
async fn a_released_holders_sends_leave_the_next_holder_room() {
    let backend = Backend::answering(std::iter::repeat_n(Answer::Never, 8));
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_RELEASED_KEY",
        vec![(
            "ROLE inbox",
            from_launch(&[
                "ch = runtime.channels['inbox']\n\
                 held = [asyncio.create_task(ch.send(f'm{i}')) for i in range(16)]\n\
                 await asyncio.sleep(0)",
                "print(await runtime.channels['inbox'].send('hi'))",
            ]),
        )],
        |builder| builder.channel(inbox().call_timeout(Duration::from_secs(2)), given),
    )
    .await;
    let first = ready(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&first));
    let sends = || {
        let events = tree.events.lock().expect("events");
        kind(&events, "message.sent", Some(&subject)).len()
    };
    until("the first holder's sixteen sends", || sends() == 16).await;
    until("the backend's eight", || backend.sent().len() == 8).await;
    within(first.release()).await;
    let second = ready(&tree, holder(&["inbox"])).await;
    let next = format!("agent/{}", agent_of(&second));
    let (_, events, _) = tree.end().await;
    let sent = backend.sent();
    let hi = sent.last().expect("the second holder's send").item;
    assert_eq!(exec_outputs(&events, &next), [format!("{}\n", hi.get())]);
    assert_eq!(
        sent.len(),
        9,
        "eight of the first holder's and the second's one"
    );
    assert_eq!(sent[8].body, json!("hi"));
    let refused: Vec<Value> = data_of(&events, "channel.item.outcome", &subject)
        .into_iter()
        .filter(|outcome| outcome["outcome"] == "refused")
        .map(|outcome| outcome["reason"].clone())
        .collect();
    assert_eq!(refused.len(), 8, "{refused:?}");
}

/// The host checks a send against the channel's contract whatever the holder's code did to its
/// endpoint: a body on a channel that sends nothing, one of the wrong type, and one its type's
/// schema refuses are each refused, and the backend is given none of them.
#[tokio::test]
async fn the_host_holds_a_send_to_its_channels_contract() {
    let backend = Arc::new(Backend::default());
    let given = |backend: &Arc<Backend>| -> Arc<dyn ChannelBackend> { backend.clone() };
    let code = "import outrig\n\
                anything = outrig.schema._contract((outrig.schema._JSON,), 'a bypass')\n\
                for name, value in (('nothing', 'x'), ('text', 1), ('typed', {'text': 'hi'})):\n\
                \x20   ch = runtime.channels[name]\n\
                \x20   ch._sending = anything\n\
                \x20   try:\n\
                \x20       await ch.send(value)\n\
                \x20   except outrig.SendRefused as e:\n\
                \x20       print(name, e.reason)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_CONTRACT_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| {
            builder
                .channel(
                    ChannelDefinition::new("nothing").sends(Contract::Nothing),
                    given(&backend),
                )
                .channel(ChannelDefinition::new("text"), given(&backend))
                .channel(
                    ChannelDefinition::new("typed")
                        .sends(Contract::Typed("app.messages:Note".to_string())),
                    given(&backend),
                )
        },
    )
    .await;
    let root = ready(&tree, holder(&["nothing", "text", "typed"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, _) = tree.end().await;
    assert!(backend.sent().is_empty(), "{:?}", backend.sent());
    let printed = exec_outputs(&events, &subject).join("");
    for expected in [
        "nothing channel \"nothing\" sends nothing",
        "text channel \"text\" sends str, not that",
        "typed channel \"typed\" sends app.messages:Note",
    ] {
        assert!(printed.contains(expected), "{expected}: {printed}");
    }
}

/// `room()` on a full window resolves once the session closes, as a push then would answer
/// `Closed`: an owner waiting for room is not left waiting on a session that has ended.
#[tokio::test]
async fn room_resolves_as_the_session_closes() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_ROOM_CLOSE_KEY",
        vec![("ROLE inbox", running(&["pass"]))],
        |builder| builder.channel(inbox().bound(1), backend),
    )
    .await;
    let channel = tree.session.channel("inbox").expect("declared");
    within(channel.deliver(Inbound::message("m-1", "alice", json!("one"))))
        .await
        .expect("queued");
    assert_eq!(
        within(channel.deliver(Inbound::message("m-2", "alice", json!("two")))).await,
        Err(DeliverError::Full)
    );
    let room = tokio::spawn(channel.room());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!room.is_finished(), "no room while the window is full");
    tree.end().await;
    within(room)
        .await
        .expect("room resolves once the session closes");
    assert!(matches!(
        within(channel.deliver(Inbound::message("m-3", "alice", json!("three")))).await,
        Err(DeliverError::Closed(_))
    ));
}

/// A channel the owner closes refuses each send still waiting for its turn at the backend: the
/// eight the backend holds end as it answers them, and the ninth is never handed over, though
/// their ends free its turn.
#[tokio::test]
async fn a_close_refuses_the_sends_waiting_for_the_backend() {
    let backend = Backend::answering(std::iter::repeat_n(Answer::Never, 8));
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "import outrig\nch = runtime.channels['inbox']\n\
                held = [asyncio.create_task(ch.send(f'm{i}')) for i in range(9)]\n\
                try:\n    await held[8]\nexcept outrig.ChannelClosed as e:\n    print('closed', e.reason)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_CLOSE_QUEUED_KEY",
        vec![("ROLE inbox", from_launch(&[code]))],
        |builder| builder.channel(inbox().call_timeout(Duration::from_secs(2)), given),
    )
    .await;
    let root = constructed(&tree, holder(&["inbox"])).await;
    let subject = format!("agent/{}", agent_of(&root));
    let ended = |outcome: &str| {
        let events = tree.events.lock().expect("events");
        data_of(&events, "channel.item.outcome", &subject)
            .into_iter()
            .filter(|ended| ended["outcome"] == outcome)
            .map(|ended| ended["reason"].clone())
            .collect::<Vec<Value>>()
    };
    tree.seen("message.sent", 9).await;
    until("the backend's eight", || backend.sent().len() == 8).await;
    tree.session
        .channel("inbox")
        .expect("declared")
        .close("peer gone");
    tree.seen("exec.completed", 1).await;
    until("the eight given up on", || ended("uncertain").len() == 8).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(backend.sent().len(), 8, "{:?}", backend.sent());
    assert_eq!(ended("refused"), [json!("peer gone")]);
    let (_, events, _) = tree.end().await;
    assert_eq!(exec_outputs(&events, &subject), ["closed peer gone\n"]);
}

/// A consumed notice the backend acknowledges only once the agent that received the item has
/// given the channel up is evented under that agent, not the channel's next holder.
#[tokio::test]
async fn a_late_acknowledgment_is_the_receivers() {
    let backend = Arc::new(Backend::default());
    *backend.refusing.lock().unwrap() = true;
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_LATE_NOTICE_KEY",
        vec![(
            "ROLE inbox",
            from_launch(&["d = await runtime.channels['inbox'].receive()", "pass"]),
        )],
        |builder| {
            builder.channel(
                inbox().deliver(Inbound::message("m-1", "alice", json!("one"))),
                given,
            )
        },
    )
    .await;
    let first = ready(&tree, holder(&["inbox"])).await;
    let receiver = format!("agent/{}", agent_of(&first));
    until("a notice", || !backend.notices().is_empty()).await;
    within(first.release()).await;
    let second = ready(&tree, holder(&["inbox"])).await;
    let next = format!("agent/{}", agent_of(&second));
    *backend.refusing.lock().unwrap() = false;
    tree.seen("channel.consumed", 1).await;
    let (_, events, _) = tree.end().await;
    assert_eq!(data_of(&events, "channel.consumed", &receiver).len(), 1);
    assert!(data_of(&events, "channel.consumed", &next).is_empty());
}

/// A message that arrives while a round's code runs is told of in its result; the provider
/// refusing the call that carried that result -- and again without it -- the model never took
/// it, so the message opens the next round rather than waiting unread for another arrival.
#[tokio::test]
async fn a_message_told_only_in_a_refused_result_opens_the_next_round() {
    let backend = Arc::new(Backend::default());
    let given: Arc<dyn ChannelBackend> = backend.clone();
    let code = "ch = runtime.channels['inbox']\nd = await ch.receive()\n\
                while ch.pending() == 0:\n    await asyncio.sleep(0.05)";
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_REFUSED_RESULT_KEY",
        vec![(
            "ROLE inbox",
            ready_then(scripted(vec![
                submit("c1", &py(code)),
                Style::Messages.empty("refusal"),
                Style::Messages.empty("refusal"),
                text_reply("told"),
            ])),
        )],
        |builder| builder.channel(inbox(), given),
    )
    .await;
    let root = ready(&tree, holder(&["inbox"])).await;
    let seq = root.status().seq;
    let channel = tree.session.channel("inbox").expect("declared");
    within(channel.deliver(Inbound::message("m-1", "alice", json!("one"))))
        .await
        .expect("queued");
    until("m-1 taken", || !backend.notices().is_empty()).await;
    within(channel.deliver(Inbound::message("m-2", "alice", json!("two"))))
        .await
        .expect("queued");
    let told = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = root.status();
            if matches!(status.state, AgentState::Idle { round } if round > 2) {
                return status;
            }
            root.next_stop(status.seq).await;
        }
    })
    .await
    .expect("the unread message opens a round past the refused one");
    assert!(told.seq > seq);
    let subject = format!("agent/{}", agent_of(&root));
    let (_, events, requests) = tree.end().await;
    let failed = data_of(&events, "model.round.failed", &subject);
    assert!(
        failed.iter().any(|failed| failed["round"] == 2),
        "the round whose result told of m-2 was refused: {failed:?}"
    );
    let last = openings(&requests, "ROLE inbox");
    assert_eq!(
        last.last().map(String::as_str),
        Some("[outrig] 1 message is waiting on runtime.channels[\"inbox\"]."),
        "{last:?}"
    );
}

/// What happens on a channel no agent holds yet, in a session with no primary, is the session's:
/// an initial item's `message.sent`, a push the full window refuses, and the owner's close carry
/// no subject, and nothing in the log names a primary the session never had.
#[tokio::test]
async fn an_unheld_channels_events_are_the_sessions() {
    let backend: Arc<dyn ChannelBackend> = Arc::new(Backend::default());
    let (tree, _app) = holding(
        "OUTRIG_TEST_CHANNEL_UNHELD_EVENTS_KEY",
        vec![("ROLE inbox", running(&["pass"]))],
        |builder| {
            builder.channel(
                inbox()
                    .bound(1)
                    .deliver(Inbound::message("m-0", "alice", json!("first"))),
                backend,
            )
        },
    )
    .await;
    let channel = tree.session.channel("inbox").expect("declared");
    assert_eq!(
        within(channel.deliver(Inbound::message("m-1", "alice", json!("second")))).await,
        Err(DeliverError::Full)
    );
    channel.close("peer gone");
    let (_, events, _) = tree.end().await;
    for (what, count) in [
        ("message.sent", 2),
        ("message.refused", 1),
        ("channel.closed", 1),
    ] {
        let found = kind(&events, what, None);
        assert_eq!(found.len(), count, "{what}");
        for event in found {
            assert_eq!(event.subject, None, "{what}");
            if what != "channel.closed" {
                assert_eq!(data(event)["to"], "none", "{what}");
            }
        }
    }
    let primary: Vec<&str> = events
        .iter()
        .filter(|event| event.subject.as_ref().map(|s| s.as_str()) == Some("agent/primary"))
        .map(|event| event.kind())
        .collect();
    assert!(primary.is_empty(), "{primary:?}");
}
