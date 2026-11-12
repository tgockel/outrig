//! A session's events, as values: what each subscriber is handed, in the order
//! the session published them.
//!
//! One stream rather than one per subject, because an agent's timeline is a
//! causal chain -- a model call produced this code, which printed this, which
//! sent this message -- and recovering that order by joining separate records
//! fails exactly when it is needed. Each event is numbered as it is published,
//! and the numbers are unique and gap-free, so [`Event::id`] is its place in
//! the session.
//!
//! A subscription is code its owner runs in its own process; nothing listens
//! for connections. It never slows the session: a subscription that falls
//! behind loses its oldest events, and is told how many before the next one it
//! receives ([`Received::Missed`]). Since the numbering has no gaps, every id a
//! subscription did not see is counted that way.
//!
//! The payloads are the ones `events.jsonl` records under `data`, field for
//! field: an event's [`Payload`] serializes to exactly that object, and
//! [`Event::kind`] is its `type` after `org.outrig.`. The [event log
//! reference](https://tgockel.github.io/outrig/reference/events.html) says what
//! each one means. Every type here is `#[non_exhaustive]`: a later release adds
//! fields and kinds without a break, so an embedder reads these and never
//! builds them.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::config::RoleAlternation;

/// How many events a subscription holds for its reader before it starts
/// losing the oldest, unless its owner chose otherwise.
pub const DEFAULT_CAPACITY: usize = 4096;

/// One thing the session did, or that was done to it, as it was published.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Event {
    /// Its place in the session's sequence, from 1.
    pub id: u64,
    /// When it was published.
    pub time: SystemTime,
    /// The agent it belongs to, or `None` for one about the session or the
    /// interpreter every agent shares.
    pub subject: Option<Subject>,
    /// What it says: the fields `events.jsonl` records as its `data`.
    pub payload: Payload,
}

impl Event {
    /// The event's type, without the `org.outrig.` prefix: `exec.submitted`,
    /// say.
    pub fn kind(&self) -> &'static str {
        self.payload.kind()
    }
}

/// The agent an event belongs to, as the interpreter's protocol names it:
/// `agent/primary` for the primary agent, and `agent/<id>` for a child it
/// launched -- `agent/c1`, say.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Subject(Cow<'static, str>);

impl Subject {
    pub(crate) const PRIMARY: Subject = Subject(Cow::Borrowed(crate::events::PRIMARY_SUBJECT));

    /// The agent the interpreter's protocol names `agent`; or `owner`, the
    /// program on the host that owns the session, beside the agents.
    pub(crate) fn agent(agent: &str) -> Subject {
        match agent {
            crate::python::host::PRIMARY => Subject::PRIMARY,
            crate::python::host::OWNER => Subject(Cow::Borrowed(crate::python::host::OWNER)),
            _ => Subject(Cow::Owned(format!("agent/{agent}"))),
        }
    }

    /// The subject as the event log writes it: `agent/primary`, `agent/c1`, or
    /// `owner`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub(crate) const fn new(id: u64) -> Self {
                Self(id)
            }

            /// The number, as the event log writes it.
            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// One submission, for the life of the session's interpreter.
    ExecId
);
id_type!(
    /// One message on an agent's channel.
    MessageId
);
id_type!(
    /// One model call the loop decided to make, however many requests it took:
    /// a call that was retried or moved to another model keeps its id.
    CallId
);
id_type!(
    /// One request actually sent to a provider. A call that was retried or
    /// moved has an attempt for each request, under the one [`CallId`]. Unique
    /// within its session.
    AttemptId
);
id_type!(
    /// One request agent code made of a hosted object -- an attribute read, a
    /// call, a step of an iteration -- for the life of the session. Every event
    /// about the request carries it.
    RequestId
);
id_type!(
    /// One request an agent made of a child it launched -- a submission, or a
    /// request on one of the child's request channels -- for the life of the
    /// session's interpreter. Every event about the request carries it.
    AgentRequestId
);
id_type!(
    /// One invocation of a skill through `outrig.skills.invoke`, for the life
    /// of the session's interpreter: its events carry it, and so do the typed
    /// calls and the hosted requests its code made while it ran.
    InvocationId
);
id_type!(
    /// One instance of an agent class -- a subclass of `outrig.Agent` -- for
    /// the life of the session's interpreter. Every event about the instance
    /// carries it.
    AgentInstanceId
);
id_type!(
    /// One call agent code made of a service's method, for the life of the
    /// session, counted from 1. Every event about the call carries it, and so
    /// does the [`CallContext`](super::CallContext) its service is given.
    ServiceCallId
);

/// Seconds, as the event log writes a duration.
fn seconds<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
}

/// Seconds, or `null` where there is no duration.
fn seconds_opt<S: Serializer>(
    duration: &Option<Duration>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match duration {
        Some(duration) => seconds(duration, serializer),
        None => serializer.serialize_none(),
    }
}

/// What an event says. Each serializes to the `data` the event log records
/// for its kind.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Payload {
    // Model view: what a model call was presented, or produced.
    /// What every model call carries besides the conversation.
    #[non_exhaustive]
    ModelInstructions {
        /// The `[models.<name>]` row every call is tried against first: for an
        /// alias, the first of its models this build can reach.
        model: String,
        /// The system prompt: OutRig's orientation, the agent's configured
        /// preamble, then the repository's instructions for agents when the
        /// agent loaded any.
        preamble: String,
        /// The tools every call offers the model.
        tools: Vec<ToolDefinition>,
        /// That model's reply ceiling, after its published ceiling filled it in
        /// or lowered it, or `None` when none is sent.
        max_tokens: Option<u32>,
    },
    /// A turn of the conversation, as it was committed. `messages` are rig's
    /// message JSON, whose form can change with a rig upgrade.
    #[non_exhaustive]
    TurnCommitted {
        /// Its id, from 0, in commit order.
        turn: u64,
        /// The round it belongs to.
        round: u32,
        /// Whether the round ended while its calls ran, so OutRig's note saying
        /// so stands in for each missing result.
        incomplete: bool,
        /// The turn's messages: a round's first turn begins with the round's
        /// opening.
        messages: Vec<Value>,
    },
    /// The text of a model's reply, as the call that made it returned. A
    /// reply with no text says nothing here.
    #[non_exhaustive]
    ModelText {
        /// Its text parts, joined.
        text: String,
    },
    /// What a model call was sent of the conversation, and why.
    ModelCall(ModelCall),
    /// Python the model submitted, as it goes to run.
    #[non_exhaustive]
    ExecSubmitted {
        /// The id its execution's other events carry.
        execid: ExecId,
        /// The Python, as submitted.
        source: String,
    },

    // Execution diagnostics: the runtime's own state.
    /// What the session is doing, as it changes: `starting` first, and
    /// `reported` last.
    #[non_exhaustive]
    SessionState {
        /// What it is doing from now on.
        state: SessionState,
    },
    /// What the session's shutdown found: its report, less how its events
    /// were delivered, which is final only once this has been published.
    #[non_exhaustive]
    SessionReport {
        /// What closed the session to new work.
        closed_by: super::ClosedBy,
        /// Whether what the session started is proven stopped.
        stopped: super::Stopped,
        /// Each execution running at the close, and how it ended.
        executions: Vec<super::ExecutionOutcome>,
        /// Each request to a binding that ended after the close, in the order
        /// they ended.
        hosted: Vec<super::HostedCall>,
        /// Each call of a service's method that ended after the close, in the
        /// order they ended.
        services: Vec<super::ServiceCall>,
        /// How the process holding the session's bindings stopped, or `None`
        /// when the session had none.
        bindings: Option<super::BindingStop>,
        /// Each request made of a child agent that was open at the close, and
        /// how it ended.
        requests: Vec<super::RequestOutcome>,
        /// What the report adds up to.
        verdict: super::Verdict,
    },
    /// An agent started: a child's is recorded as it is launched.
    #[non_exhaustive]
    AgentStarted {
        /// The `[models.<name>]` row every call of the agent is tried against
        /// first.
        model: String,
        /// The interpreter's Python version.
        python: String,
        /// The name of the container the interpreter runs in.
        container: String,
        /// The most tool calls one of its rounds may make: its `tool-call-max`.
        tool_call_max: u64,
        /// The most bytes one tool result may hold before it is cut: its
        /// `tool-result-max`.
        tool_result_max: u64,
    },
    /// The agent stopped: as the session's shutdown stops the container, or for
    /// a child as its kernel ends after its release.
    #[non_exhaustive]
    AgentStopped {},
    /// A round of the event's subject began.
    #[non_exhaustive]
    RoundStarted {
        /// The round's number. A round that commits no turn leaves its number
        /// to the next, and each agent counts its own.
        round: u32,
    },
    /// A submission that was not run: `holder` held the interpreter, or the
    /// session was closed to new work.
    #[non_exhaustive]
    ExecRefused {
        /// The id the submission was given.
        execid: ExecId,
        /// The execution that held the interpreter, or `None` when the session
        /// was closed to new work.
        holder: Option<ExecId>,
        /// Why it was not run.
        reason: Refusal,
    },
    /// How an execution ended, when its result arrived.
    #[non_exhaustive]
    ExecCompleted {
        /// The execution, as `exec.submitted` numbered it.
        execid: ExecId,
        /// How it ended.
        status: ExecStatus,
        /// How long it took, from its submission.
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        /// What it printed, bounded.
        output: String,
        /// The bytes of output past the bound.
        dropped: u64,
        /// Its traceback, or `None`.
        error: Option<String>,
        /// Output earlier executions wrote after they reported, delivered with
        /// this result.
        background: Vec<Background>,
    },
    /// An execution raised `MemoryError`, beside its `exec.completed`, whose
    /// `error` holds the traceback.
    #[non_exhaustive]
    MemoryExhausted {
        /// The execution that raised it.
        execid: ExecId,
    },
    /// A cancel was sent to an execution, which raises `CancelledError` at the
    /// await it is suspended on.
    #[non_exhaustive]
    ExecCancelSent {
        /// The execution it was sent to.
        execid: ExecId,
    },
    /// An interrupt was sent to an execution, which raises `KeyboardInterrupt`
    /// in agent code that has stopped yielding or is blocked in a call.
    #[non_exhaustive]
    ExecInterruptSent {
        /// The execution it was sent for.
        execid: ExecId,
        /// Whether it was sent for code found keeping a CPU busy, which it
        /// reaches in whatever agent code holds the event loop, not only in
        /// this execution's.
        runaway: bool,
    },
    /// A liveness check of an execution's event loop went unanswered.
    #[non_exhaustive]
    ExecProbeFailed {
        /// The execution that was running.
        execid: ExecId,
        /// What the check found the loop doing.
        verdict: ProbeVerdict,
    },
    /// OutRig stopped waiting for an execution, which keeps the interpreter
    /// until it ends.
    #[non_exhaustive]
    ExecAbandoned {
        /// The execution it stopped waiting for.
        execid: ExecId,
        /// Why it stopped waiting.
        why: Abandoned,
    },
    /// What the agent's namespace held when a liveness check of a running
    /// execution was answered: names and types, never values.
    #[non_exhaustive]
    InventoryObserved {
        /// The execution that was running.
        execid: ExecId,
        /// The names it held, each with its value's type, at most 200.
        names: Vec<Held>,
        /// How many names it held in all.
        total: u64,
        /// How many it held past those listed.
        more: u64,
    },
    /// A `runtime.wait` the agent's code has been suspended in for a second:
    /// what it waits on, `on`, and `more`, those past what is listed; and
    /// `elapsed`, how long it had lasted when it was recorded, from which its
    /// `timeout` counts.
    #[non_exhaustive]
    ExecWaitStarted {
        /// The execution whose code waits.
        execid: ExecId,
        /// The interpreter's id for the wait, which its `exec.wait.ended`
        /// carries too.
        wait: u64,
        /// What it waits on, at most 50 entries.
        on: Vec<Awaited>,
        /// How many it waits on past those in `on`.
        more: u64,
        /// When it returns, as asyncio names it.
        return_when: ReturnWhen,
        /// Its timeout, or `None` when it has none.
        #[serde(serialize_with = "seconds_opt")]
        timeout: Option<Duration>,
        /// How long it had lasted when it was recorded, from which `timeout`
        /// counts.
        #[serde(serialize_with = "seconds")]
        elapsed: Duration,
    },
    /// A wait that was recorded starting has ended: `how`; `done`, what was
    /// done by then; `channels`, those a message arrived on; and `error`, the
    /// exception that ended it, when one did.
    #[non_exhaustive]
    ExecWaitEnded {
        /// The execution whose code waited.
        execid: ExecId,
        /// The interpreter's id for the wait, as its `exec.wait.started` gave
        /// it.
        wait: u64,
        /// How it ended.
        how: WaitEnd,
        /// What was done by then, described as `exec.wait.started` describes
        /// what it waits on.
        done: Vec<Awaited>,
        /// The channels a message was waiting on when it ended, for `message`;
        /// otherwise empty.
        channels: Vec<String>,
        /// The name of the exception that ended it, for `raised`; otherwise
        /// `None`.
        error: Option<String>,
        /// How long it lasted, from when it began.
        #[serde(serialize_with = "seconds")]
        duration: Duration,
    },
    /// A tool result was cut to fit `tool-result-max`, with a marker saying
    /// what was cut.
    #[non_exhaustive]
    ToolResultTruncated {
        /// The execution whose result it is.
        execid: ExecId,
        /// The bytes of the text that was cut, before the cut.
        size: u64,
        /// The bytes that text had room for: `tool-result-max`, less what the
        /// result holds whole ahead of it.
        max: u64,
        /// The bytes of that text kept, ahead of the marker.
        kept: u64,
    },
    /// The agent's code promoted turns, with `runtime.context.promote`: each is
    /// chosen for later model calls until it is demoted.
    #[non_exhaustive]
    ContextPromoted {
        /// The turns' ids, as the agent's code named them.
        turns: Vec<u64>,
    },
    /// The agent's code demoted turns, with `runtime.context.demote`, ending
    /// their promotion.
    #[non_exhaustive]
    ContextDemoted {
        /// The turns' ids, as the agent's code named them.
        turns: Vec<u64>,
    },
    /// A line no execution can be billed for: what `os.write(1, ...)` wrote,
    /// say, or a child process started outside every execution.
    #[non_exhaustive]
    OutputUnattributed {
        /// The line, cut past 4 KiB.
        text: String,
    },
    /// A line the interpreter wrote about itself.
    #[non_exhaustive]
    InterpreterDiagnostic {
        /// The line, without its `outrig-interpreter:` prefix.
        text: String,
    },
    /// The interpreter's output closed, for `cause`. `expected` when the
    /// session's shutdown stopped it; otherwise it died, which ends the
    /// session.
    #[non_exhaustive]
    InterpreterExited {
        /// Why its output closed, such as how its process exited and the last
        /// of its stderr.
        cause: String,
        /// Whether the session's shutdown stopped it.
        expected: bool,
    },
    /// A binding a repository's config declared did not start, because the
    /// operator did not approve that declaration or nobody could be asked; the
    /// session runs without it. `reason` says which, and how to approve it.
    #[non_exhaustive]
    BindingRefused {
        /// The name the binding was declared under.
        binding: String,
        /// The config file that declared it, or `None` when it has none.
        declared_in: Option<std::path::PathBuf>,
        /// Why it did not start, and how to approve it: the operator did not
        /// approve the declaration, nobody could be asked, or the session had
        /// no approver.
        reason: String,
    },
    /// The process holding the session's bindings exited while the session
    /// ran, for `cause`. The session goes on without them: each call it was
    /// running is `unknown`, and a later call to any of them fails.
    #[non_exhaustive]
    BindingExited {
        /// The names of the bindings the process held.
        bindings: Vec<String>,
        /// How the process ended, such as `it was killed by SIGKILL`.
        cause: String,
    },
    /// The session's application code was provisioned as it started: read
    /// from `path` -- or from the bytes its owner gave, when that is `None` --
    /// copied whole to a directory named for `digest`, which the interpreter
    /// imports from as `mount`, with the packages `requires` names installed
    /// in `packages`. The modules `imports` names are imported next, each
    /// answered by an `application.imported`.
    #[non_exhaustive]
    ApplicationProvisioned {
        /// The directory its owner gave, as given, or `None` for an archive.
        path: Option<std::path::PathBuf>,
        /// The first 16 hex digits of a BLAKE3 digest of its files' paths and
        /// contents: its identity, the same for a directory and for an archive
        /// holding the same files.
        digest: String,
        /// What its owner calls it, which nothing compares.
        label: Option<String>,
        /// Where the interpreter imports it from.
        mount: String,
        /// The requirement set its owner declared, sorted.
        requires: Vec<String>,
        /// Where the interpreter imports the requirement set from, or `None`
        /// when it is empty.
        packages: Option<String>,
        /// The modules imported as the session starts, in order.
        imports: Vec<String>,
    },
    /// A module of the session's application code was imported as the session
    /// started, or failed to be, which fails the start. `code` and `reason`
    /// say why, when it failed.
    #[non_exhaustive]
    ApplicationImported {
        /// The module, as its owner named it.
        module: String,
        /// Why it did not import -- `import-failed`, `module-missing` or
        /// `native-extension-unavailable` -- or `None` when it did.
        code: Option<String>,
        /// What was raised and the last 16 KiB of its traceback, its control
        /// characters but line breaks and tabs escaped, or `None` when it
        /// imported.
        reason: Option<String>,
    },
    /// A root its session's owner asked for was not constructed in its
    /// kernel, which was ended: no child started, and no model was called.
    #[non_exhaustive]
    AgentConstructFailed {
        /// The class as the owner named it, `module:qualname`.
        class: String,
        /// The part of `class` before its colon.
        module: String,
        /// The part of `class` after its colon.
        qualname: String,
        /// Why: `module-missing`, `import-failed`, `native-extension-unavailable`,
        /// `class-missing`, `not-an-agent-class`, `invalid-argument`, `raised`
        /// for a constructor or hook that raised, or `closing` when the
        /// session closed first.
        code: String,
        /// What was wrong and the last 16 KiB of the traceback, when there is
        /// one, its control characters but line breaks and tabs escaped.
        reason: String,
    },
    /// What the session's skill sources hold, as the session started: each
    /// skill served, and why any other was left out, shadowed by an earlier
    /// source's, or served with a warning. Read from each skill's metadata
    /// alone, with no Python run.
    #[non_exhaustive]
    SkillDiscovered {
        /// Each skill served.
        skills: Vec<DiscoveredSkill>,
        /// What discovery found wrong with a skill, or with a source.
        diagnostics: Vec<SkillDiagnostic>,
    },
    /// A skill's package was imported in the interpreter -- again, after
    /// `outrig.skills.reload`, when `reload` -- by `agent`'s code, or by code
    /// outside every execution when that is `None`. `digest` is its files'
    /// when it was imported; `code` and `reason` say why the import failed,
    /// when it did.
    #[non_exhaustive]
    SkillImported {
        /// The skill whose package was imported.
        skill: String,
        /// The source serving it.
        source: String,
        /// The agent whose code imported it, as the interpreter's protocol
        /// names it, or `None` for code outside every execution.
        agent: Option<String>,
        /// The digest of its files then, or `None` when the host gave none.
        digest: Option<String>,
        /// Whether `outrig.skills.reload` imported it again.
        reload: bool,
        /// Why the import failed, as a code such as `unsatisfied-dependency`,
        /// or `None` when it did not fail.
        code: Option<String>,
        /// Why the import failed, in full, its control characters escaped, or
        /// `None` when it did not fail.
        reason: Option<String>,
    },
    /// A `/name` the user typed reached the event's subject on its user
    /// channel as message `message`, naming `skill`, which `source` serves.
    /// Nothing of the skill runs until the agent's code invokes it.
    #[non_exhaustive]
    SkillDirectiveReceived {
        /// The skill the `/name` names.
        skill: String,
        /// The source serving it.
        source: String,
        /// The id `message.sent` gave the message that carried it.
        message: MessageId,
    },
    /// The event's subject's code is calling `skill`'s entry through
    /// `outrig.skills.invoke`, as `invocation`. `source` serves the skill,
    /// whose files had `digest` when its package was imported. `params` is a
    /// bounded preview of the `<Skill>Params` the entry is called with, or
    /// `None` while the session's events are not observed; `by` says whether a
    /// child parsed them from what the user typed, in the typed call `call`,
    /// or the caller passed them; `bindings` names the bindings its parameters
    /// were filled from.
    #[non_exhaustive]
    SkillInvocationStarted {
        /// Its id, which the typed calls and the hosted requests made inside it
        /// carry.
        invocation: InvocationId,
        /// The skill whose entry is called.
        skill: String,
        /// The source serving it.
        source: String,
        /// The digest of the skill's files when its package was imported.
        digest: String,
        /// The JSON of the `<Skill>Params` the entry is called with, cut to
        /// 1,024 characters, or `None` when the session's events are not
        /// observed.
        params: Option<String>,
        /// Where the parameters came from.
        by: ParamsBy,
        /// The typed call that parsed them from what the user typed, or `None`
        /// when the caller passed them.
        call: Option<AgentRequestId>,
        /// The bindings the entry's parameters were filled from.
        bindings: Vec<String>,
    },
    /// The entry of `invocation` ended, `duration` after it was called:
    /// `result` names the type of what it returned and `exception` the type
    /// of what it raised. `usage` is what the model requests of every child
    /// made inside the invocation used, its parser's included, or `None` when
    /// none reported anything.
    #[non_exhaustive]
    SkillInvocationFinished {
        /// The invocation, as its `skill.invocation.started` numbered it.
        invocation: InvocationId,
        /// How the entry ended.
        outcome: InvocationEnd,
        /// How long the entry ran.
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        /// The qualified name of the type of what it returned, or `None` when
        /// it did not return.
        result: Option<String>,
        /// The qualified name of the type of what it raised, or `None` when it
        /// did not raise.
        exception: Option<String>,
        /// What the model requests of every child made inside the invocation
        /// used, or `None` when none reported anything.
        usage: Option<Usage>,
    },
    /// A child agent was launched, by `parent`; the event's subject is the
    /// child's.
    #[non_exhaustive]
    AgentSpawned {
        /// What the spawn called it: a label, which children can share.
        name: String,
        /// The subject of the agent that launched it, or `owner` for the host
        /// program's.
        parent: String,
        /// Which kind of child it is.
        kind: ChildKind,
        /// The `[models.<name>]` row as the spawn named it, or as its parent
        /// runs: `default-model` for the host program's.
        model: String,
        /// How deep it is: the main agent's children, and the host program's,
        /// are at 1.
        depth: u32,
        /// Its inputs, bound as variables before its first round.
        inputs: Vec<InputBrief>,
        /// What it was given to act through, the view it is told of: its
        /// constructor's facilities, or its parent's it inherited; `None` when
        /// its view is the session's, every binding.
        facilities: Option<Vec<FacilityBrief>>,
        /// The channels a request child answers requests on.
        channels: Vec<ChannelBrief>,
    },
    /// A request was sent to the event's subject, a child: a submission, when
    /// `channel` is `None`, or a request on that channel. `body` is a bounded
    /// preview of what was sent -- a submission's inputs, by name.
    #[non_exhaustive]
    AgentRequestSent {
        /// The request's id, which its other events carry.
        request: AgentRequestId,
        /// The channel it was sent on, or `None` for a submission.
        channel: Option<String>,
        /// The subject of the agent that made it, or `owner` for the host
        /// program's.
        sender: String,
        /// A preview of what was sent, cut to 1,024 characters: a submission's
        /// inputs, by name.
        body: String,
        /// How many answers that fail their check it takes before it ends
        /// `rejected`.
        attempts_max: u32,
    },
    /// The child took the request: its code received it from its channel, or
    /// it was given the submission at the start of `round`.
    #[non_exhaustive]
    AgentRequestReceived {
        /// The request the child took.
        request: AgentRequestId,
        /// The child's round running then, or `None` when none was.
        round: Option<u32>,
    },
    /// The child answered the request, with `value` as a bounded preview. An
    /// answer that failed its check is followed by `agent.request.invalid`.
    #[non_exhaustive]
    AgentRequestReplied {
        /// The request the child answered.
        request: AgentRequestId,
        /// Which answer this is, counted from 1.
        attempt: u32,
        /// A preview of the answer, cut to 1,024 characters.
        value: String,
    },
    /// The child's answer to a request failed its check.
    #[non_exhaustive]
    AgentRequestInvalid {
        /// The request the child answered.
        request: AgentRequestId,
        /// Which answer failed, as `agent.request.replied` counted it.
        attempt: u32,
        /// What failed the check, each with its path.
        problems: Vec<String>,
    },
    /// A work child reported progress on the submission it works on, which
    /// its sender reads from the handle. Settles nothing.
    #[non_exhaustive]
    AgentRequestProgress {
        /// The submission the child works on.
        request: AgentRequestId,
        /// The progress, as text cut to 1,024 characters.
        body: String,
    },
    /// The child failed the request on purpose, saying why.
    #[non_exhaustive]
    AgentRequestFailed {
        /// The request the child failed.
        request: AgentRequestId,
        /// Why, cut to 1,024 characters.
        message: String,
    },
    /// The request was cancelled, before or after the child received it: by
    /// its sender, or, when `reason` says why, by the host, the child's rounds
    /// having ended without answering it after a warning.
    #[non_exhaustive]
    AgentRequestCancelled {
        /// The request that was cancelled.
        request: AgentRequestId,
        /// Whether the child had taken it.
        received: bool,
        /// Why the host cancelled it, or `None` when its sender did.
        reason: Option<String>,
    },
    /// How the request ended, how many answers it took, and the child's
    /// rounds it spanned. `cause` is how a model call of the child's failed
    /// and ended a round of them, when one did and the host cancelled a
    /// submission for the child's rounds ending without completing it: the
    /// text its sender's exception carries as `__cause__`.
    #[non_exhaustive]
    AgentRequestSettled {
        /// The request that ended.
        request: AgentRequestId,
        /// How it ended.
        outcome: RequestEnd,
        /// How many answers it took.
        attempts: u32,
        /// The child's rounds it was open during.
        rounds: Vec<u32>,
        /// The text of a model call's failure that ended a round of the
        /// child's, for a submission the host then cancelled, or `None`. Its
        /// sender's exception carries it as `__cause__`.
        cause: Option<String>,
    },
    /// An answer to a request that had already ended, refused.
    #[non_exhaustive]
    AgentRequestRefused {
        /// The request the answer was for.
        request: AgentRequestId,
        /// How the request had ended.
        ended: RequestEnd,
    },
    /// The event's subject released `released`; that ended `ended`, every
    /// child in their subtrees, and settled `requests`.
    #[non_exhaustive]
    AgentReleased {
        /// The children the subject's code named, by id.
        released: Vec<String>,
        /// Those, and every child below them, each ended or found wedged.
        ended: Vec<String>,
        /// The requests that ended `released` with them.
        requests: Vec<AgentRequestId>,
    },
    /// The event's subject, a released child, did not stop: its code never
    /// yielded, so its thread runs on until the session ends.
    #[non_exhaustive]
    AgentWedged {},
    /// A typed call -- `@outrig.agent`'s -- that `parent`'s code made, whose
    /// child is the event's subject: the declaration's `__module__` and
    /// `__qualname__`, and the SHA-256 digest of the source that declared it,
    /// or `None` when none could be read; and the skill invocation the call
    /// was made inside, if any. The submission's request events follow under
    /// the same `request`.
    #[non_exhaustive]
    AgentCallStarted {
        /// The id of the submission the call gives the child, which the call's
        /// handle carries.
        request: AgentRequestId,
        /// The subject of the agent whose code made the call.
        parent: String,
        /// The declaration's `__module__`.
        module: String,
        /// The declaration's `__qualname__`.
        qualname: String,
        /// A SHA-256 of the source that declared it -- its module's file, or
        /// the submission's source for a function declared in one -- or `None`
        /// when none could be read.
        digest: Option<String>,
        /// The skill invocation the call was made inside, or `None`.
        invocation: Option<InvocationId>,
    },
    /// The call ended and its child was released: how its submission ended,
    /// what every model request the child made used, or `None` when none
    /// reported anything, and the skill invocation it was made inside, if any.
    /// `value` is a bounded preview of the result it was accepted with, or
    /// `None` when it was not accepted or the session's events are not
    /// observed.
    #[non_exhaustive]
    AgentCallSettled {
        /// The call's submission, as `agent.call.started` named it.
        request: AgentRequestId,
        /// How the submission ended.
        outcome: RequestEnd,
        /// What every model request the child made used, or `None` when none
        /// reported anything.
        usage: Option<Usage>,
        /// The skill invocation the call was made inside, or `None`.
        invocation: Option<InvocationId>,
        /// A preview of the result it was accepted with, cut to 1,024
        /// characters, or `None` when it was not accepted or the session's
        /// events are not observed.
        value: Option<String>,
    },
    /// An agent class's instance, made by `parent`'s code, launched its child,
    /// the event's subject: the class's `__module__` and `__qualname__`, the
    /// SHA-256 digest of the source that declared it, or `None` when none
    /// could be read, the child's inputs, its request channels, the sections
    /// its class contributed to the child's instructions, and whether the
    /// child has the instance as its `self`.
    #[non_exhaustive]
    AgentInstanceStarted {
        /// The instance's id, which its other events carry.
        instance: AgentInstanceId,
        /// The subject of the agent whose code constructed it, or `owner` for
        /// one the host program constructed.
        parent: String,
        /// The class's `__module__`.
        module: String,
        /// The class's `__qualname__`.
        qualname: String,
        /// A SHA-256 of the source that declared the class -- its module's
        /// file, or the submission's source for a class declared in one -- or
        /// `None` when none could be read.
        digest: Option<String>,
        /// The child's inputs, bound as variables before its first round.
        inputs: Vec<InputBrief>,
        /// The child's facilities, as `agent.spawned` carries them.
        facilities: Option<Vec<FacilityBrief>>,
        /// The child's request channels, one per request method.
        channels: Vec<ChannelBrief>,
        /// The sections its class's `__outrig_instructions__` returned, in the
        /// order the child's instructions carry them: none for a class that
        /// defines no such hook.
        contributions: Vec<ContributionBrief>,
        /// Whether the child has the instance as its `self`, which every
        /// instance an agent's code constructs is: `false` for one a host
        /// program constructed, which stays in the program. Serialized as
        /// `self`.
        #[serde(rename = "self")]
        has_self: bool,
    },
    /// The instance's child ended its orienting round, the one it has as it
    /// launches, and is ready; or `error` says why it cannot have that round.
    #[non_exhaustive]
    AgentInstanceReady {
        /// The instance whose child it is.
        instance: AgentInstanceId,
        /// Why the child cannot have that round -- the token budget spent, or a
        /// model that cannot answer -- or `None` when it had it.
        error: Option<String>,
    },
    /// The instance released its child, `how`, which settled `requests`.
    /// `usage` is what the child's own model requests used, or `None` when
    /// none reported anything.
    #[non_exhaustive]
    AgentInstanceReleased {
        /// The instance that was released.
        instance: AgentInstanceId,
        /// How it was released.
        how: InstanceRelease,
        /// The requests on the child that the release, or the session's close,
        /// ended.
        requests: Vec<AgentRequestId>,
        /// What every model request the child made used, or `None` when none
        /// reported anything.
        usage: Option<Usage>,
    },
    /// The instance was collected: its release follows, unless the session's
    /// close had released it. Only an instance a host program constructed is
    /// collected so; one whose child has it as `self` never is.
    #[non_exhaustive]
    AgentInstanceCollected {
        /// The instance that was collected.
        instance: AgentInstanceId,
    },
    /// The handle of `request`, made through the instance, was collected
    /// while the request was open. The request goes on.
    #[non_exhaustive]
    AgentInstanceHandleCollected {
        /// The instance the request was made through.
        instance: AgentInstanceId,
        /// The request whose handle was collected.
        request: AgentRequestId,
    },

    // Integration audit: what crossed to a provider, or across a channel.
    /// A round ended as its model yielded, or as a limit stopped it.
    /// `usage` and `attempts` are the round's own; `descendants` the requests
    /// the children it launched made while it ran, and `total` what the two
    /// used together, each attempt counted once.
    #[non_exhaustive]
    ModelRoundCompleted {
        /// The round that ended.
        round: u32,
        /// Why OutRig ended it, such as the tool-call cap, or `None` when it
        /// did not.
        stopped: Option<String>,
        /// The round's tokens: the sum over its attempts, or `None` when none
        /// reported any.
        usage: Option<Usage>,
        /// Each of its model calls, and what the attempt that answered it used.
        calls: Vec<CallUsage>,
        /// Every request the round sent, failed ones included.
        attempts: Vec<AttemptId>,
        /// The largest `input_tokens` any of its calls reported, or `None`.
        input_tokens_max: Option<u64>,
        /// The requests sent while it ran for the child agents its code
        /// launched, and theirs below them.
        descendants: Vec<AttemptId>,
        /// Its own usage and its descendants', each attempt counted once, or
        /// `None` when none reported any.
        total: Option<Usage>,
    },
    /// A round ended by an error, with what it had used so far.
    #[non_exhaustive]
    ModelRoundFailed {
        /// The round that failed.
        round: u32,
        /// The error that ended it.
        error: String,
        /// Each model call it had made, and what the attempt that answered it
        /// used.
        calls: Vec<CallUsage>,
        /// The round's tokens so far: the sum over its attempts, or `None` when
        /// none reported any.
        usage: Option<Usage>,
        /// Every request the round sent, failed ones included.
        attempts: Vec<AttemptId>,
        /// The requests sent while it ran for the child agents its code
        /// launched, and theirs below them.
        descendants: Vec<AttemptId>,
        /// Its own usage and its descendants', each attempt counted once, or
        /// `None` when none reported any.
        total: Option<Usage>,
    },
    /// A round ended while a model call was in flight, with what it had used so
    /// far: its future was dropped, the call outlasted a release, or the
    /// session ended.
    #[non_exhaustive]
    ModelRoundDropped {
        /// The round that was dropped.
        round: u32,
        /// Each model call it had made, and what the attempt that answered it
        /// used.
        calls: Vec<CallUsage>,
        /// The round's tokens so far: the sum over its attempts, or `None` when
        /// none reported any.
        usage: Option<Usage>,
        /// Every request the round sent, failed ones included.
        attempts: Vec<AttemptId>,
        /// The requests sent while it ran for the child agents its code
        /// launched, and theirs below them.
        descendants: Vec<AttemptId>,
        /// Its own usage and its descendants', each attempt counted once, or
        /// `None` when none reported any.
        total: Option<Usage>,
    },
    /// Requests a child the round launched made after the round had ended:
    /// spend added to that round's tree once its total was published.
    #[non_exhaustive]
    ModelRoundUsage {
        /// The round the requests belong to.
        round: u32,
        /// The requests its descendants sent after it ended.
        attempts: Vec<AttemptId>,
        /// What they used together, or `None` when none reported any.
        usage: Option<Usage>,
    },
    /// One request a provider was sent, and how it ended.
    ModelAttempt(ModelAttempt),
    /// A model call failed and will be made again, after a wait.
    #[non_exhaustive]
    ModelRetry {
        /// The `[models.<name>]` row being retried.
        model: String,
        /// The try that failed, counted from 1 within its layer's retries.
        attempt: u32,
        /// How long until the next try.
        #[serde(serialize_with = "seconds")]
        delay: Duration,
        /// Why it failed, without the provider's response body: an HTTP status,
        /// a connection error, or an unusable response.
        error: String,
        /// The model call being retried.
        call_id: CallId,
        /// The request that failed.
        attempt_id: AttemptId,
    },
    /// A model call moved to the next of an alias's models, once the one before
    /// had failed past its retries or could not take the call.
    #[non_exhaustive]
    ModelFailover {
        /// The `[models.<name>]` row given up.
        from: String,
        /// The `[models.<name>]` row the call moves to.
        to: String,
        /// Why `from` was given up.
        error: String,
        /// The model call that moved.
        call_id: CallId,
        /// The last request `from` was sent, or `None` when it was passed
        /// over without one.
        attempt_id: Option<AttemptId>,
    },
    /// `model` refused call `call_id` for what it carried, ending an empty
    /// reply with `finish_reason`, and the call is sent to it again with a
    /// notice in place of each result of `turn`, the turn it answers. Once
    /// that is answered, every later call is sent the notice too.
    #[non_exhaustive]
    ModelWithheld {
        /// The `[models.<name>]` row that refused the call.
        model: String,
        /// The model call that was refused.
        call_id: CallId,
        /// The turn whose results are left out: the one the call answers.
        turn: u64,
        /// The finish reason the refusal ended with, such as `content_filter`
        /// or `refusal`.
        finish_reason: String,
    },
    /// A usage record that arrived after its attempt's event, accepted: it
    /// replaces that attempt's null usage, once.
    #[non_exhaustive]
    ModelUsageReplaced {
        /// The model call the attempt served.
        call_id: CallId,
        /// The attempt whose usage it replaces.
        attempt_id: AttemptId,
        /// What the record says the attempt used.
        usage: Usage,
    },
    /// A usage record that changed nothing.
    #[non_exhaustive]
    ModelUsageRefused {
        /// The model call the attempt served, or `None` for an attempt the
        /// session has no record of.
        call_id: Option<CallId>,
        /// The attempt the record names.
        attempt_id: AttemptId,
        /// What the record says the attempt used.
        usage: Usage,
        /// Why it changed nothing.
        reason: UsageRefusal,
    },
    /// A message was put on a channel.
    #[non_exhaustive]
    MessageSent {
        /// The id OutRig gives it, which its other events carry.
        message: MessageId,
        /// The channel it was put on.
        channel: String,
        /// Who sent it: `user`, or the agent's subject, such as
        /// `agent/primary`.
        from: String,
        /// Who it is for: `user`, or the agent's subject.
        to: String,
        /// Its text, as the channel carries it.
        body: String,
    },
    /// A message the other end did not take.
    #[non_exhaustive]
    MessageRefused {
        /// The id OutRig gave it.
        message: MessageId,
        /// The channel it was put on.
        channel: String,
        /// Who sent it: `user`, or the agent's subject, such as
        /// `agent/primary`.
        from: String,
        /// Who it was for: `user`, or the agent's subject.
        to: String,
        /// Why it was not taken.
        reason: String,
    },
    /// The other end took a message. For a message to the agent, that is its
    /// code receiving it, which can be well after it arrived.
    #[non_exhaustive]
    MessageReceived {
        /// The id its `message.sent` gave it.
        message: MessageId,
        /// The channel it was put on.
        channel: String,
        /// Who sent it: `user`, or the agent's subject, such as
        /// `agent/primary`.
        from: String,
        /// Who took it: `user`, or the agent's subject.
        to: String,
    },
    /// The host program injected a live object it holds, which every kernel
    /// reaches as `outrig.bindings[binding]`: `type` is the qualified name of
    /// its type.
    #[non_exhaustive]
    BindingInjected {
        /// The name agents reach it by, in `outrig.bindings`.
        binding: String,
        /// The qualified name of the object's type.
        #[serde(rename = "type")]
        type_name: String,
    },
    /// A request agent code made of a hosted object, as it arrived: published
    /// before anything is decided about it. `by` says who saw it -- the
    /// bindings process, which received it, or, for one it never received,
    /// the owner's relay or the interpreter. `execid` and `invocation` are the
    /// execution and the skill invocation it was made in, as the interpreter
    /// said: diagnostic context, which the agent's own code can change.
    #[non_exhaustive]
    HostedReceived {
        /// The id OutRig gives it, which its other events carry.
        request: RequestId,
        /// The agent whose code made it, as the interpreter's protocol names
        /// it.
        agent: String,
        /// The binding whose object it was made of.
        binding: String,
        /// The execution that made it, or `None`.
        execid: Option<ExecId>,
        /// The skill invocation it was made inside, or `None`.
        invocation: Option<InvocationId>,
        /// What it asks of its target.
        operation: Operation,
        /// The attribute it names -- or, for `cmp`, the operator -- or `None`.
        member: Option<String>,
        /// The qualified name of its target's type on the host, or `None`.
        target: Option<String>,
        /// A preview of each positional argument.
        arguments: Vec<String>,
        /// A preview of each keyword argument, by name.
        keywords: std::collections::BTreeMap<String, String>,
        /// The request a callback made it under, or `None`.
        parent: Option<RequestId>,
        /// Who saw it arrive.
        by: ReceivedBy,
    },
    /// A request's target is about to be invoked: published before it is.
    #[non_exhaustive]
    HostedDispatched {
        /// The request, as its `hosted.received` numbered it.
        request: RequestId,
        /// The agent whose code made it, as the interpreter's protocol names
        /// it.
        agent: String,
        /// The binding whose object it was made of.
        binding: String,
    },
    /// How a request ended.
    #[non_exhaustive]
    HostedEnded {
        /// The request, as its `hosted.received` numbered it.
        request: RequestId,
        /// The agent whose code made it, as the interpreter's protocol names
        /// it.
        agent: String,
        /// The binding whose object it was made of.
        binding: String,
        /// How it ended.
        outcome: HostedOutcome,
        /// Why, for an outcome that has a reason; otherwise `None`.
        reason: Option<HostedReason>,
        /// What refused it, or `None`.
        detail: Option<String>,
        /// The qualified name of what a `raised` call raised, or `None`.
        exception: Option<String>,
        /// The host's traceback, its last 16 KiB, which agent code does not
        /// get; or `None`.
        traceback: Option<String>,
        /// Whether the caller was interrupted while the target ran.
        interrupted: bool,
        /// How long it took, from its receipt.
        #[serde(serialize_with = "seconds")]
        duration: Duration,
    },
    /// A hosted library called back a callable agent code passed it, in the
    /// call `parent`.
    #[non_exhaustive]
    HostedCallback {
        /// The request the callable was passed in.
        parent: RequestId,
        /// The agent whose code passed the callable, as the interpreter's
        /// protocol names it.
        agent: String,
        /// The binding whose library called it.
        binding: String,
    },
    /// A service the owner bound, as the session started: what agents reach as
    /// `outrig.bindings[service]`, and how its methods were described.
    #[non_exhaustive]
    ServiceBound {
        /// The name agents reach it by.
        service: String,
        /// What kind of service it is.
        kind: ServiceKind,
        /// How many methods it describes.
        methods: usize,
        /// Its description's generation, 1 at the start.
        generation: u64,
        /// The methods whose argument schema did not compile, by exact name:
        /// each is called with its arguments unchecked.
        unchecked: Vec<String>,
    },
    /// A service's description changed while the session ran: what agents are
    /// told of it from now on.
    #[non_exhaustive]
    ServiceDescribed {
        /// The name agents reach it by.
        service: String,
        /// The new description's generation.
        generation: u64,
        /// Which methods changed, by exact name.
        changes: ServiceChanges,
        /// The methods whose argument schema did not compile, by exact name.
        unchecked: Vec<String>,
    },
    /// A call agent code made of a service's method, as it arrived: published
    /// before anything is decided about it. `execid` and `invocation` are the
    /// execution and the skill invocation it was made in, as the interpreter
    /// said: diagnostic context, which the agent's own code can change.
    #[non_exhaustive]
    ServiceCallReceived {
        /// The id OutRig gives it, which its other events carry.
        call: ServiceCallId,
        /// The service whose method it calls.
        service: String,
        /// What kind of service that is.
        kind: ServiceKind,
        /// The method, by its exact name.
        method: String,
        /// The generation of the description the caller's method came from.
        generation: u64,
        /// The agent whose code made it, as the interpreter's protocol names it.
        agent: String,
        /// The execution that made it, or `None`.
        execid: Option<ExecId>,
        /// The skill invocation it was made inside, or `None`.
        invocation: Option<InvocationId>,
        /// How long its service has to answer it, from its receipt.
        #[serde(serialize_with = "seconds")]
        deadline: Duration,
        /// A preview of its arguments, cut to 1,024 characters, when the
        /// session's service policy records bodies; otherwise `None`.
        arguments: Option<String>,
    },
    /// A call's service is about to be called: published before it is.
    #[non_exhaustive]
    ServiceCallDispatched {
        /// The call, as its `service.call.received` numbered it.
        call: ServiceCallId,
        /// The service whose method it calls.
        service: String,
        /// The method, by its exact name.
        method: String,
        /// The agent whose code made it.
        agent: String,
    },
    /// A dispatched call was told to stop: its service's cancellation token
    /// was triggered.
    #[non_exhaustive]
    ServiceCallCancelSent {
        /// The call, as its `service.call.received` numbered it.
        call: ServiceCallId,
        /// The service whose method it calls.
        service: String,
        /// The agent whose code made it.
        agent: String,
        /// Why.
        reason: ServiceReason,
    },
    /// How a call ended.
    #[non_exhaustive]
    ServiceCallEnded {
        /// The call, as its `service.call.received` numbered it.
        call: ServiceCallId,
        /// The service whose method it calls.
        service: String,
        /// What kind of service that is.
        kind: ServiceKind,
        /// The method, by its exact name.
        method: String,
        /// The agent whose code made it.
        agent: String,
        /// The execution that made it, or `None`.
        execid: Option<ExecId>,
        /// How it ended.
        outcome: ServiceOutcome,
        /// What the service said went wrong, for a `raised` call; otherwise
        /// `None`.
        category: Option<super::ServiceErrorCategory>,
        /// Why, for a `refused`, `cancelled` or `unknown` call; otherwise
        /// `None`.
        reason: Option<ServiceReason>,
        /// The failing JSON path of arguments refused as invalid, or what a
        /// result refused as too large measured; otherwise `None`.
        detail: Option<String>,
        /// The size of the result its service returned, as JSON, or `None`
        /// when it returned none.
        bytes: Option<u64>,
        /// How long it took, from its receipt.
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        /// A preview of its result, cut to 1,024 characters, when the
        /// session's service policy records bodies; otherwise `None`.
        body: Option<String>,
    },
}

impl Payload {
    /// The event's type, without the `org.outrig.` prefix.
    pub fn kind(&self) -> &'static str {
        match self {
            Payload::ModelInstructions { .. } => "model.instructions",
            Payload::TurnCommitted { .. } => "turn.committed",
            Payload::ModelText { .. } => "model.text",
            Payload::ModelCall(_) => "model.call",
            Payload::ExecSubmitted { .. } => "exec.submitted",
            Payload::SessionState { .. } => "session.state",
            Payload::SessionReport { .. } => "session.report",
            Payload::AgentStarted { .. } => "agent.started",
            Payload::AgentStopped {} => "agent.stopped",
            Payload::RoundStarted { .. } => "round.started",
            Payload::ExecRefused { .. } => "exec.refused",
            Payload::ExecCompleted { .. } => "exec.completed",
            Payload::MemoryExhausted { .. } => "memory.exhausted",
            Payload::ExecCancelSent { .. } => "exec.cancel.sent",
            Payload::ExecInterruptSent { .. } => "exec.interrupt.sent",
            Payload::ExecProbeFailed { .. } => "exec.probe.failed",
            Payload::ExecAbandoned { .. } => "exec.abandoned",
            Payload::InventoryObserved { .. } => "inventory.observed",
            Payload::ExecWaitStarted { .. } => "exec.wait.started",
            Payload::ExecWaitEnded { .. } => "exec.wait.ended",
            Payload::ToolResultTruncated { .. } => "tool.result.truncated",
            Payload::ContextPromoted { .. } => "context.promoted",
            Payload::ContextDemoted { .. } => "context.demoted",
            Payload::OutputUnattributed { .. } => "output.unattributed",
            Payload::InterpreterDiagnostic { .. } => "interpreter.diagnostic",
            Payload::InterpreterExited { .. } => "interpreter.exited",
            Payload::BindingRefused { .. } => "binding.refused",
            Payload::BindingInjected { .. } => "binding.injected",
            Payload::ModelRoundCompleted { .. } => "model.round.completed",
            Payload::ModelRoundFailed { .. } => "model.round.failed",
            Payload::ModelRoundDropped { .. } => "model.round.dropped",
            Payload::ModelAttempt(_) => "model.attempt",
            Payload::ModelRetry { .. } => "model.retry",
            Payload::ModelFailover { .. } => "model.failover",
            Payload::ModelWithheld { .. } => "model.withheld",
            Payload::ModelUsageReplaced { .. } => "model.usage.replaced",
            Payload::ModelUsageRefused { .. } => "model.usage.refused",
            Payload::MessageSent { .. } => "message.sent",
            Payload::MessageRefused { .. } => "message.refused",
            Payload::MessageReceived { .. } => "message.received",
            Payload::BindingExited { .. } => "binding.exited",
            Payload::ApplicationProvisioned { .. } => "application.provisioned",
            Payload::ApplicationImported { .. } => "application.imported",
            Payload::AgentConstructFailed { .. } => "agent.construct.failed",
            Payload::SkillDiscovered { .. } => "skill.discovered",
            Payload::SkillImported { .. } => "skill.imported",
            Payload::SkillDirectiveReceived { .. } => "skill.directive.received",
            Payload::SkillInvocationStarted { .. } => "skill.invocation.started",
            Payload::SkillInvocationFinished { .. } => "skill.invocation.finished",
            Payload::AgentSpawned { .. } => "agent.spawned",
            Payload::AgentRequestSent { .. } => "agent.request.sent",
            Payload::AgentRequestReceived { .. } => "agent.request.received",
            Payload::AgentRequestReplied { .. } => "agent.request.replied",
            Payload::AgentRequestInvalid { .. } => "agent.request.invalid",
            Payload::AgentRequestFailed { .. } => "agent.request.failed",
            Payload::AgentRequestCancelled { .. } => "agent.request.cancelled",
            Payload::AgentRequestProgress { .. } => "agent.request.progress",
            Payload::AgentRequestSettled { .. } => "agent.request.settled",
            Payload::AgentRequestRefused { .. } => "agent.request.refused",
            Payload::AgentReleased { .. } => "agent.released",
            Payload::AgentWedged {} => "agent.wedged",
            Payload::AgentCallStarted { .. } => "agent.call.started",
            Payload::AgentCallSettled { .. } => "agent.call.settled",
            Payload::AgentInstanceStarted { .. } => "agent.instance.started",
            Payload::AgentInstanceReady { .. } => "agent.instance.ready",
            Payload::AgentInstanceReleased { .. } => "agent.instance.released",
            Payload::AgentInstanceCollected { .. } => "agent.instance.collected",
            Payload::AgentInstanceHandleCollected { .. } => "agent.instance.handle.collected",
            Payload::ModelRoundUsage { .. } => "model.round.usage",
            Payload::HostedReceived { .. } => "hosted.received",
            Payload::HostedDispatched { .. } => "hosted.dispatched",
            Payload::HostedEnded { .. } => "hosted.ended",
            Payload::HostedCallback { .. } => "hosted.callback",
            Payload::ServiceBound { .. } => "service.bound",
            Payload::ServiceDescribed { .. } => "service.described",
            Payload::ServiceCallReceived { .. } => "service.call.received",
            Payload::ServiceCallDispatched { .. } => "service.call.dispatched",
            Payload::ServiceCallCancelSent { .. } => "service.call.cancel.sent",
            Payload::ServiceCallEnded { .. } => "service.call.ended",
        }
    }

    /// The agent the event belongs to, or `None` for one about the session or
    /// the interpreter every agent shares. An event that names no agent of its
    /// own belongs to `emitter`, the agent whose handle published it.
    pub(crate) fn subject(&self, emitter: &Subject) -> Option<Subject> {
        match self {
            Self::SessionState { .. }
            | Self::SessionReport { .. }
            | Self::OutputUnattributed { .. }
            | Self::InterpreterDiagnostic { .. }
            | Self::InterpreterExited { .. }
            | Self::BindingRefused { .. }
            | Self::BindingInjected { .. }
            | Self::BindingExited { .. }
            | Self::ApplicationProvisioned { .. }
            | Self::ApplicationImported { .. }
            | Self::AgentConstructFailed { .. }
            | Self::SkillDiscovered { .. }
            | Self::ServiceBound { .. }
            | Self::ServiceDescribed { .. } => None,
            // An import is the agent's whose code made it, which may be none.
            Self::SkillImported { agent, .. } => agent.as_deref().map(Subject::agent),
            Self::HostedReceived { agent, .. }
            | Self::HostedDispatched { agent, .. }
            | Self::HostedEnded { agent, .. }
            | Self::HostedCallback { agent, .. }
            | Self::ServiceCallReceived { agent, .. }
            | Self::ServiceCallDispatched { agent, .. }
            | Self::ServiceCallCancelSent { agent, .. }
            | Self::ServiceCallEnded { agent, .. } => Some(Subject::agent(agent)),
            _ => Some(emitter.clone()),
        }
    }
}

/// What a hosted request asked of its target: one of the handlers of the
/// transport's protocol, named as `0003-16`'s inventory names them.
/// `getroot`, `inspect`, `del`, `ping`, `close` and `pickle` are bookkeeping,
/// recorded only when they are refused; the rest are boundary requests,
/// recorded every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Operation {
    /// A read of the attribute the event's `member` names.
    Getattr,
    /// A write of the attribute the event's `member` names.
    Setattr,
    /// A deletion of the attribute the event's `member` names.
    Delattr,
    /// A call of the target.
    Call,
    /// A call of the target's attribute `member`, read and called in one
    /// request: how a `for` loop reaches `__iter__` and `__next__`.
    Callattr,
    /// The target's `repr`.
    Repr,
    /// The target's `str`.
    Str,
    /// A comparison of the target with another object, through the operator
    /// method `member` names on the target's type.
    Cmp,
    /// The target's `hash`.
    Hash,
    /// The target's attribute names, as `dir` lists them, less those a request
    /// may not name.
    Dir,
    /// Whether an object of the connection's is an instance of the target, a
    /// class.
    Instancecheck,
    /// The next items of the target, an iterator, several in one request.
    Buffiter,
    /// A slice of the target, through the item method `member` names, or
    /// failing that its old-style slice method.
    Oldslicing,
    /// The target's `__exit__`, as a `with` block in the agent's code ends,
    /// with the exception that ended it.
    Ctxexit,
    /// A check that the connection answers, which echoes what it was sent.
    Ping,
    /// The connection's close.
    Close,
    /// A request for the connection's root: the binding's object.
    Getroot,
    /// A request for the methods of an object of the host's, which its proxy's
    /// class is built from.
    Inspect,
    /// A request for the target, pickled. Always refused.
    Pickle,
    /// A release of references the interpreter held to an object of the host's.
    Del,
    /// A request type outside the protocol's table, which is refused.
    Unknown,
}

impl Operation {
    /// The operation the protocol's lines name `name`.
    pub(crate) fn named(name: &str) -> Operation {
        match name {
            "getattr" => Operation::Getattr,
            "setattr" => Operation::Setattr,
            "delattr" => Operation::Delattr,
            "call" => Operation::Call,
            "callattr" => Operation::Callattr,
            "repr" => Operation::Repr,
            "str" => Operation::Str,
            "cmp" => Operation::Cmp,
            "hash" => Operation::Hash,
            "dir" => Operation::Dir,
            "instancecheck" => Operation::Instancecheck,
            "buffiter" => Operation::Buffiter,
            "oldslicing" => Operation::Oldslicing,
            "ctxexit" => Operation::Ctxexit,
            "ping" => Operation::Ping,
            "close" => Operation::Close,
            "getroot" => Operation::Getroot,
            "inspect" => Operation::Inspect,
            "pickle" => Operation::Pickle,
            "del" => Operation::Del,
            _ => Operation::Unknown,
        }
    }

    /// Whether a request of this operation is bookkeeping, recorded only when it is refused.
    pub(crate) fn bookkeeping(self) -> bool {
        matches!(
            self,
            Operation::Ping
                | Operation::Close
                | Operation::Getroot
                | Operation::Inspect
                | Operation::Pickle
                | Operation::Del
        )
    }
}

/// Who saw a hosted request arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReceivedBy {
    /// The bindings process, which received it.
    Binding,
    /// The owner's relay, which refused it, or lost it with the process or
    /// its connection, before the process received it as the request its
    /// label numbers; described from what the interpreter said of it.
    Relay,
    /// The interpreter, which never sent it.
    Interpreter,
}

/// How a hosted request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostedOutcome {
    /// Its target was invoked and returned.
    Returned,
    /// Its target was invoked and raised.
    Raised,
    /// Its target was never invoked: something refused it.
    Refused,
    /// Its target was never invoked: its caller was interrupted, its
    /// connection closed, or the session closed, while it waited.
    Cancelled,
    /// It was sent and no reply came: its process was killed, or died.
    /// Whether its target was invoked is open.
    Unknown,
}

/// Why a hosted request was refused, cancelled, or ended unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostedReason {
    /// The bindings process's checks refused it; `detail` says which.
    Interception,
    /// The session was closed to new work.
    AdmissionClosed,
    /// The bindings process was not reading, and its queue had no room; or
    /// its agent had as many requests in flight as the relay holds.
    QueueFull,
    /// The bindings process had exited.
    Exited,
    /// Its caller was interrupted, or cancelled.
    Interrupted,
    /// The session closed while it waited.
    Closed,
    /// The bindings process was killed at shutdown, or the host program serving it was gone.
    Killed,
    /// Its connection closed before the bindings process read it.
    Disconnected,
    /// The host program serving it was still running it when the session terminated, and
    /// nothing the engine owns could end it.
    StillRunning,
}

/// What kind of service a call reached: one an owner wrote in Rust, or one of
/// the kinds OutRig ships.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceKind {
    /// A [`Service`](super::Service) the owner bound with
    /// [`SessionBuilder::serve`](super::SessionBuilder::serve).
    Rust,
}

/// How a call of a service's method ended: the five outcomes a hosted request
/// has, so one rule reads both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceOutcome {
    /// Its service was called and returned a result.
    Returned,
    /// Its service was called and returned an error, whose category the
    /// event's `category` is.
    Raised,
    /// Its service was never called, but for a result too large, which it
    /// returned and its caller was not given: the reason says which.
    Refused,
    /// Its service was never called: its caller was interrupted, or the
    /// session closed, before it was dispatched.
    Cancelled,
    /// Its service was called and abandoned -- told to stop, given its grace,
    /// and aborted -- so what it did is open.
    Unknown,
}

/// Why a call of a service's method was refused, cancelled, abandoned, or told
/// to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ServiceReason {
    /// The session was closed to new work.
    AdmissionClosed,
    /// The service's description, as it stands, has no method of that name.
    UnknownMethod,
    /// The arguments did not match the method's schema; `detail` is the
    /// first failing JSON path.
    InvalidArguments,
    /// The result was larger, as JSON, than the session's policy lets one
    /// cross; `detail` says how large.
    ResultTooLarge,
    /// The call's deadline passed.
    Deadline,
    /// Its caller was interrupted.
    Interrupted,
    /// The session's drain ended with it still running.
    Closed,
}

/// The methods a service's new description added, removed and changed, each
/// by exact name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct ServiceChanges {
    /// Methods the old description did not have.
    pub added: Vec<String>,
    /// Methods the new description does not have.
    pub removed: Vec<String>,
    /// Methods both have, described differently.
    pub changed: Vec<String>,
}

/// A tool a model call carries, as the provider is told of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolDefinition {
    /// What the model calls it by.
    pub name: String,
    /// What the model is told it does.
    pub description: String,
    /// The JSON Schema of its arguments.
    pub parameters: Value,
}

/// One model call's manifest: what it was sent of the conversation, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelCall {
    /// This assembly's place among the session's, from 0. A call moved to
    /// another model is assembled again, under the same `call_id`.
    pub call: u64,
    /// The model call it was assembled for, which names the call in every model
    /// event.
    pub call_id: CallId,
    /// The round the call was made in.
    pub round: u32,
    /// What it was held to.
    pub budget: CallBudget,
    /// The whole request's estimated tokens, before `budget.estimate_scale`.
    pub estimate: u64,
    /// The turns it was sent, oldest first, each with why it was chosen.
    pub carried: Vec<Chosen>,
    /// The turns chosen but left out for size.
    pub evicted: Vec<Chosen>,
    /// The turns chosen, and fitting, but left out so that the user's and the
    /// model's turns alternate; empty unless `budget.role_alternation` is
    /// [`RoleAlternation::Strict`].
    pub withheld: Vec<Chosen>,
    /// The round's opening, as rig's message JSON, when it was the call's
    /// prompt.
    pub opening: Option<Value>,
    /// Where one role follows itself in what was sent.
    pub adjacent: Vec<Repeat>,
    /// The parts of carried turns the call's provider cannot take, which it was
    /// not sent.
    pub left_out: Vec<LeftOut>,
    /// The carried turns whose results a provider refused a call for, each
    /// result sent as a notice instead.
    pub refused: Vec<RefusedTurn>,
    /// The room, in estimated tokens, the call's view was cut down to, when
    /// this call cut it: what was chosen did not fit, so it was chosen again
    /// in half the room beside the call's prompt, and later calls are sent
    /// what it kept and every turn committed since until that does not fit
    /// either.
    pub cut: Option<u64>,
}

/// What a call was held to, in tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallBudget {
    /// The `[models.<name>]` row the call was assembled for.
    pub model: String,
    /// The context window the call was held to, in tokens.
    pub window: u32,
    /// Where `window` came from.
    pub window_source: WindowSource,
    /// The tokens of the window held back for the reply.
    pub reserve: u32,
    /// What every call carries besides the conversation -- the system prompt
    /// and the tool definitions -- in estimated tokens.
    pub overhead: u64,
    /// What the call's `estimate` was multiplied by to be held to the window: 1.0
    /// is the estimate as it stands, and above it the model's provider has
    /// reported more input tokens for a request than the estimate said.
    pub estimate_scale: f64,
    /// The reply ceiling the call carried, or `None` when none was sent.
    pub max_tokens: Option<u32>,
    /// Whether the model's provider requires the user's and the model's turns
    /// to alternate, as its provider row says.
    pub role_alternation: RoleAlternation,
}

/// Where the window a call was held to came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WindowSource {
    /// The model row's `context-window`.
    Configured,
    /// What the model's provider reported when the session asked, because the
    /// row sets no `context-window`.
    Provider,
    /// A window of 128,000 tokens, which the row and the provider both left
    /// unsaid.
    Assumed,
    /// A window lowered from the one before it, because the model's provider
    /// rejected a request as over its input limit.
    Rejected,
}

/// A turn a call carried or left out, and why it was chosen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Chosen {
    /// The turn, by its id.
    pub turn: u64,
    /// Why it was chosen.
    pub why: Why,
}

/// Why a turn was chosen for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Why {
    /// The turn the call answers.
    Latest,
    /// An earlier turn of the call's own round.
    Round,
    /// A turn the agent's code promoted.
    Promoted,
    /// One of the conversation's first rounds.
    First,
    /// One of the rounds just before this one.
    Recent,
}

/// Where one role follows itself in what a call was sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Repeat {
    /// The turn that begins with the repeat, or `None` for the round's
    /// opening.
    pub turn: Option<u64>,
    /// The role that follows itself.
    pub role: Role,
}

/// Whose side of the conversation a message is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    /// The user's side, which carries tool results too.
    User,
    /// The model's side.
    Assistant,
}

/// A part of a carried turn a call was not sent, by its place: the turn, the
/// message within it, and the part within that.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct LeftOut {
    /// The turn, by its id.
    pub turn: u64,
    /// The message, counted from 0 within the turn's `messages`.
    pub message: u64,
    /// The part, counted from 0 within the message's `content`.
    pub part: u64,
}

/// A carried turn whose results a provider refused a call for, which the call
/// was sent `notice` in place of each of.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct RefusedTurn {
    /// The turn, by its id.
    pub turn: u64,
    /// What each of its tool results was sent as: why, and where the results
    /// are.
    pub notice: String,
}

/// What the session is doing, as an owner would show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SessionState {
    /// The container and the interpreter are coming up.
    Starting,
    /// No round is running.
    Idle,
    /// A round is in progress, and its model is being called.
    RoundRunning,
    /// Within a round, a submission is running in the interpreter.
    Executing,
    /// From the close of admission until the shutdown report exists.
    Closing,
    /// The shutdown report exists. The last event of a session.
    Reported,
}

/// Why a submission was not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Refusal {
    /// Another execution held the interpreter: the event's `holder`.
    Held,
    /// The session was closed to new work.
    Closed,
}

/// How an execution ended, as far as the host saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ExecStatus {
    /// It ran to completion.
    Ok,
    /// It raised.
    Error,
    /// The interpreter refused it, disagreeing with the host about which
    /// execution held it.
    Refused,
    /// The interpreter exited first.
    Lost,
}

/// What a liveness check found an execution's event loop doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ProbeVerdict {
    /// The loop did not answer and its thread was idle: blocked in a system
    /// call, such as a `subprocess.run` or a `time.sleep`.
    Blocked,
    /// The loop did not answer and its thread kept a CPU busy.
    Spinning,
    /// Nothing in the interpreter answered: native code holds the GIL.
    Starved,
}

/// Why the host stopped waiting for an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Abandoned {
    /// The user interrupted it twice.
    User,
    /// It kept a CPU busy without letting its loop turn.
    Runaway,
}

/// A name the agent's namespace holds, and its value's type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Held {
    /// The name, as the agent's code bound it.
    pub name: String,
    /// Its value's type, by name.
    #[serde(rename = "type")]
    pub type_name: String,
}

/// One thing a `runtime.wait` waits on, as the interpreter describes it from
/// types and fields alone, running none of the agent's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
#[non_exhaustive]
pub enum Awaited {
    /// A task: its name, and its coroutine's qualified name when it has one.
    /// A task nobody named whose only work is `asyncio.ensure_future` of a
    /// handle or a skill's invocation is listed as that handle or invocation
    /// instead.
    #[non_exhaustive]
    Task {
        /// Its name, as `asyncio.Task.get_name` gives it.
        name: String,
        /// The qualified name of its coroutine's code, or `None` when it has
        /// none.
        code: Option<String>,
    },
    /// A submission to a child agent: the request's id, and the child's id
    /// and spawn name once the child exists.
    #[non_exhaustive]
    Submission {
        /// The request's id, which its events carry as `request`, or `None`
        /// when the interpreter named none.
        id: Option<u64>,
        /// The child's id, or `None` until the child exists.
        child: Option<String>,
        /// The child's spawn name, or `None` until the child exists.
        name: Option<String>,
    },
    /// A request on a child's channel.
    #[non_exhaustive]
    Request {
        /// The request's id, which its events carry as `request`, or `None`
        /// when the interpreter named none.
        id: Option<u64>,
        /// The child's channel the request is on.
        channel: String,
        /// The child's id, or `None` until the child exists.
        child: Option<String>,
        /// The child's spawn name, or `None` until the child exists.
        name: Option<String>,
    },
    /// A typed call of an `@outrig.agent` function, by its qualified name.
    #[non_exhaustive]
    Call {
        /// The id of the call's submission, which its events carry as
        /// `request`, or `None` when the interpreter named none.
        id: Option<u64>,
        /// The function's `__module__` and `__qualname__`, joined by a dot.
        function: String,
        /// The child's id, or `None` until the child exists.
        child: Option<String>,
        /// The child's spawn name, or `None` until the child exists.
        name: Option<String>,
    },
    /// A skill's invocation.
    #[non_exhaustive]
    Skill {
        /// The invocation's id, as `skill.invocation.started` gives it.
        id: u64,
        /// The skill whose entry the invocation calls.
        skill: String,
    },
    /// Any other future: its type's qualified name.
    #[non_exhaustive]
    Future {
        /// The qualified name of the future's type.
        #[serde(rename = "type")]
        type_name: String,
    },
}

/// When a `runtime.wait` returns, as asyncio names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ReturnWhen {
    /// `FIRST_COMPLETED`: once any of its futures is done.
    FirstCompleted,
    /// `FIRST_EXCEPTION`: once any of its futures raises, or once all are done.
    FirstException,
    /// `ALL_COMPLETED`: once all of its futures are done.
    AllCompleted,
}

/// How a recorded wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum WaitEnd {
    /// Its futures satisfied `return_when`.
    Returned,
    /// Its timeout passed.
    Timeout,
    /// A message arrived on a channel, and `MessageAvailable` was raised.
    Message,
    /// An exception ended it: a cancel, or an interrupt.
    Raised,
}

/// Output an earlier execution wrote after it reported, delivered with a
/// later one's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Background {
    /// The execution that wrote it, or `None` for the construction of a root
    /// its session's owner constructed, which ran in the agent's kernel and
    /// is no execution.
    pub id: Option<ExecId>,
    /// What it wrote, bounded.
    pub output: String,
    /// The bytes past the bound.
    pub dropped: u64,
}

/// Tokens, as a provider reported them. Where a provider reported nothing,
/// the field that holds a `Usage` is `None`, never zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct Usage {
    /// Every input token, the cache's read and write included, on every
    /// provider.
    pub input_tokens: u64,
    /// The tokens the model produced.
    pub output_tokens: u64,
    /// The provider's total, or `input_tokens` and `output_tokens` added when
    /// it gave none.
    pub total_tokens: u64,
    /// The input tokens read from the provider's prompt cache, a part of
    /// `input_tokens`.
    pub cached_input_tokens: u64,
    /// The input tokens written to the provider's prompt cache, a part of
    /// `input_tokens`.
    pub cache_creation_input_tokens: u64,
    /// The tokens the model spent reasoning, where the provider reports them.
    pub reasoning_tokens: u64,
}

impl Usage {
    /// A usage of `total_tokens` in all, for a test.
    #[cfg(test)]
    pub(crate) fn total(total_tokens: u64) -> Self {
        Self {
            total_tokens,
            ..Self::default()
        }
    }
}

/// A total is its parts added up.
impl std::iter::Sum for Usage {
    fn sum<I: Iterator<Item = Usage>>(parts: I) -> Usage {
        parts.fold(Usage::default(), |sum, part| Usage {
            input_tokens: sum.input_tokens + part.input_tokens,
            output_tokens: sum.output_tokens + part.output_tokens,
            total_tokens: sum.total_tokens + part.total_tokens,
            cached_input_tokens: sum.cached_input_tokens + part.cached_input_tokens,
            cache_creation_input_tokens: sum.cache_creation_input_tokens
                + part.cache_creation_input_tokens,
            reasoning_tokens: sum.reasoning_tokens + part.reasoning_tokens,
        })
    }
}

/// One of a round's model calls: its place in the round, the model that
/// answered it, the attempt that did, and what that attempt used.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallUsage {
    /// Its place among the round's calls, from 0.
    pub index: u64,
    /// The `[models.<name>]` row that answered it, which for an alias is not
    /// always the first.
    pub model: String,
    /// The id every event about the call carries.
    pub call_id: CallId,
    /// The request that answered it.
    pub attempt_id: AttemptId,
    /// What that request used, or `None` when the provider reported nothing.
    pub usage: Option<Usage>,
}

/// One request a provider was sent: the call it served, the settings it went
/// with, and how it ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelAttempt {
    /// The model call it was sent for.
    pub call_id: CallId,
    /// The request's own id, unique within the session.
    pub attempt_id: AttemptId,
    /// The `[models.<name>]` row it was sent for.
    pub model: String,
    /// The model the request named on the wire.
    pub identifier: String,
    /// The output-token ceiling the request carried, if any.
    pub max_tokens: Option<u32>,
    /// Why it failed, or `None` when it answered.
    pub error: Option<String>,
    /// What the provider reported it used, or `None` when it reported nothing.
    pub usage: Option<Usage>,
    /// The provider's own reason the model stopped -- `stop`, `end_turn`,
    /// `tool_calls`, `length` -- or `None` when its response named none, or no
    /// response came back.
    pub finish_reason: Option<String>,
}

/// Which kind of child an agent launched: one that takes submissions and
/// completes each, or one that answers requests on its channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ChildKind {
    /// One that takes submissions and completes each.
    Work,
    /// One that answers requests on its channels.
    Request,
}

/// One of a child's inputs, bound as a variable before its first round: its
/// name, its type, and a bounded preview of its value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct InputBrief {
    /// The variable it is bound to.
    pub name: String,
    /// Its type, as text.
    #[serde(rename = "type")]
    pub type_name: String,
    /// A preview of its JSON form, cut to 1,024 characters.
    pub preview: String,
}

/// What a facility an agent was given is: one of the session's bindings or
/// services, or a plain Python object of the interpreter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum FacilityKind {
    /// One of the session's bindings, factory or live, which each agent's
    /// kernel reaches over its own connections.
    Binding,
    /// A plain Python object, shared by the agents given it; a call on it
    /// crosses no boundary and is not evented.
    Object,
    /// One of the session's services, which each agent's kernel reaches
    /// through an object of its own, its calls attributed to that agent.
    Service,
}

/// One facility a child was given, by the name it sees it under: its kind,
/// the bound name it reaches -- `None` for a plain object -- and the qualified
/// name of its type.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct FacilityBrief {
    /// The name the child sees it under, a key of the mapping.
    pub name: String,
    /// What it is.
    pub kind: FacilityKind,
    /// The binding's or service's name, which can differ from `name`; `None`
    /// for a plain object.
    pub target: Option<String>,
    /// The qualified name of its type: the binding's object's on the host,
    /// the class generated for a service, or the plain object's.
    #[serde(rename = "type")]
    pub type_name: String,
}

/// One skill the session serves, as discovery found it: its name, the source
/// serving it, its description, the digest of its files then, and whether it
/// has a `skill.py`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DiscoveredSkill {
    /// The name it is served under.
    pub name: String,
    /// The source serving it.
    pub source: String,
    /// Its description, from its `SKILL.md`.
    pub description: String,
    /// The digest of its files at discovery.
    pub digest: String,
    /// Whether it has a `skill.py`.
    pub executable: bool,
}

/// Something discovery found wrong with a skill, or with a source when
/// `skill` is `None`. `code` is `shadowed` for a copy an earlier source's
/// serves in its place, `unknown-key` for a warning about one that is still
/// served, and otherwise why it was left out: `metadata-invalid`,
/// `duplicate-metadata-block` or `source-error`. `message` says it in full.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct SkillDiagnostic {
    /// The skill it is about, or `None` for a source that could not be listed.
    pub skill: Option<String>,
    /// The source it is about, or the one holding the skill.
    pub source: String,
    /// What kind of problem it is: `shadowed`, `unknown-key`,
    /// `metadata-invalid`, `duplicate-metadata-block`, or `source-error`.
    pub code: String,
    /// The problem in full, each control character but a line break or a tab
    /// escaped.
    pub message: String,
}

/// One of a request child's channels: its name, and the type of a request on
/// it and of the reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChannelBrief {
    /// The channel's name, which `agent.request.sent` names it by.
    pub name: String,
    /// The type of a request on it, as text.
    pub request: String,
    /// The type of a reply to one, as text.
    pub reply: String,
}

/// One section an agent class contributed to its child's instructions: its
/// title, and how long its text is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ContributionBrief {
    /// The section's title.
    pub title: String,
    /// The length of its text in bytes, as UTF-8.
    pub length: usize,
}

/// How a request an agent made of a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RequestEnd {
    /// An answer passed its check, and is its result.
    Accepted,
    /// Its answers failed their checks past the attempt limit.
    Rejected,
    /// The child failed it on purpose.
    Failed,
    /// Its sender cancelled it, or the host did: the child's rounds ended
    /// without answering it.
    Cancelled,
    /// The child, or an agent above it, was released.
    Released,
    /// The session's token budget was spent before it was answered.
    Budget,
    /// The session closed before it was answered.
    Closed,
    /// The interpreter died before it was answered.
    Terminated,
}

/// How an agent class's instance was released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InstanceRelease {
    /// Its code awaited `release()`.
    Explicit,
    /// Its `async with` was left.
    AsyncWith,
    /// It was collected unreleased: an instance a host program constructed,
    /// since one whose child has it as `self` never is.
    Collected,
    /// The session closed.
    Closing,
}

/// Where the `<Skill>Params` a skill's entry was called with came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ParamsBy {
    /// A child parsed them from the text the user typed after `/name`.
    Parser,
    /// The code that invoked the skill passed them, as `params=`.
    Caller,
}

/// How a skill's entry ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum InvocationEnd {
    /// It returned.
    Returned,
    /// It raised an exception.
    Raised,
    /// Its invocation was cancelled while it ran.
    Cancelled,
}

/// Why a usage record changed nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum UsageRefusal {
    /// The session has no attempt by that id.
    Unknown,
    /// The attempt's own event reported its usage.
    Reported,
    /// An earlier record already replaced the attempt's null usage.
    Replaced,
}

/// What a subscription hands its reader.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Received {
    /// The next event.
    Event(Arc<Event>),
    /// This many events, the oldest the subscription held, were dropped while
    /// it fell behind; the next event's id is that many past the last one
    /// received.
    Missed(u64),
}

/// Why [`Subscription::try_recv`] had nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum TryRecvError {
    /// No event is waiting yet.
    #[error("no event is waiting")]
    Empty,
    /// The session has ended its stream, and every event held here was taken.
    #[error("the session's stream has ended")]
    Closed,
}

/// One reader's view of a session's events, made by
/// [`SessionBuilder::subscribe`](super::SessionBuilder::subscribe) before the
/// session starts, so it sees every event from the first.
pub struct Subscription {
    queue: Arc<crate::events::Queue>,
}

impl Subscription {
    pub(crate) fn new(queue: Arc<crate::events::Queue>) -> Self {
        Self { queue }
    }

    /// The next event, or the count of those missed before it; `None` once
    /// the session has ended its stream and every event held here was taken.
    /// Cancel-safe: dropping the future takes nothing.
    pub async fn recv(&mut self) -> Option<Received> {
        self.queue.recv().await
    }

    /// What [`Subscription::recv`] would return, without waiting.
    pub fn try_recv(&mut self) -> Result<Received, TryRecvError> {
        self.queue.next()
    }

    /// How many events this subscription has lost in all, whether or not its
    /// reader has been told yet.
    pub fn missed(&self) -> u64 {
        self.queue.missed()
    }

    /// How many events it holds before it loses the oldest.
    pub fn capacity(&self) -> usize {
        self.queue.capacity()
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscription")
            .field("capacity", &self.capacity())
            .field("missed", &self.missed())
            .finish()
    }
}
