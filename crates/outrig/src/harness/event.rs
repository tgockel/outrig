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

use serde::{Serialize, Serializer};
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

    /// The agent the interpreter's protocol names `agent`.
    pub(crate) fn agent(agent: &str) -> Subject {
        if agent == crate::python::host::PRIMARY {
            return Subject::PRIMARY;
        }
        Subject(Cow::Owned(format!("agent/{agent}")))
    }

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

/// Seconds, as the event log writes a duration.
fn seconds<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_f64(duration.as_secs_f64())
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
        model: String,
        preamble: String,
        tools: Vec<ToolDefinition>,
        max_tokens: Option<u32>,
    },
    /// A turn of the conversation, as it was committed. `messages` are rig's
    /// message JSON, whose form can change with a rig upgrade.
    #[non_exhaustive]
    TurnCommitted {
        turn: u64,
        round: u32,
        incomplete: bool,
        messages: Vec<Value>,
    },
    /// The text of a model's reply, as the call that made it returned. A
    /// reply with no text says nothing here.
    #[non_exhaustive]
    ModelText { text: String },
    /// What a model call was sent of the conversation, and why.
    ModelCall(ModelCall),
    #[non_exhaustive]
    ExecSubmitted { execid: ExecId, source: String },

    // Execution diagnostics: the runtime's own state.
    #[non_exhaustive]
    SessionState { state: SessionState },
    /// What the session's shutdown found: its report, less how its events
    /// were delivered, which is final only once this has been published.
    #[non_exhaustive]
    SessionReport {
        closed_by: super::ClosedBy,
        stopped: super::Stopped,
        executions: Vec<super::ExecutionOutcome>,
        hosted: Vec<super::HostedCall>,
        bindings: Option<super::BindingStop>,
        requests: Vec<super::RequestOutcome>,
        verdict: super::Verdict,
    },
    #[non_exhaustive]
    AgentStarted {
        model: String,
        python: String,
        container: String,
        tool_call_max: u64,
        tool_result_max: u64,
    },
    #[non_exhaustive]
    AgentStopped {},
    #[non_exhaustive]
    RoundStarted { round: u32 },
    /// A submission that was not run: `holder` held the interpreter, or the
    /// session was closed to new work.
    #[non_exhaustive]
    ExecRefused {
        execid: ExecId,
        holder: Option<ExecId>,
        reason: Refusal,
    },
    #[non_exhaustive]
    ExecCompleted {
        execid: ExecId,
        status: ExecStatus,
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        output: String,
        dropped: u64,
        error: Option<String>,
        background: Vec<Background>,
    },
    #[non_exhaustive]
    MemoryExhausted { execid: ExecId },
    #[non_exhaustive]
    ExecCancelSent { execid: ExecId },
    #[non_exhaustive]
    ExecInterruptSent { execid: ExecId, runaway: bool },
    #[non_exhaustive]
    ExecProbeFailed {
        execid: ExecId,
        verdict: ProbeVerdict,
    },
    #[non_exhaustive]
    ExecAbandoned { execid: ExecId, why: Abandoned },
    #[non_exhaustive]
    InventoryObserved {
        execid: ExecId,
        names: Vec<Held>,
        total: u64,
        more: u64,
    },
    #[non_exhaustive]
    ToolResultTruncated {
        execid: ExecId,
        size: u64,
        max: u64,
        kept: u64,
    },
    #[non_exhaustive]
    ContextPromoted { turns: Vec<u64> },
    #[non_exhaustive]
    ContextDemoted { turns: Vec<u64> },
    #[non_exhaustive]
    OutputUnattributed { text: String },
    #[non_exhaustive]
    InterpreterDiagnostic { text: String },
    /// The interpreter's output closed, for `cause`. `expected` when the
    /// session's shutdown stopped it; otherwise it died, which ends the
    /// session.
    #[non_exhaustive]
    InterpreterExited { cause: String, expected: bool },
    /// A binding a repository's config declared did not start, because the
    /// operator did not approve that declaration or nobody could be asked; the
    /// session runs without it. `reason` says which, and how to approve it.
    #[non_exhaustive]
    BindingRefused {
        binding: String,
        declared_in: Option<std::path::PathBuf>,
        reason: String,
    },
    /// The process holding the session's bindings exited while the session
    /// ran, for `cause`. The session goes on without them: each call it was
    /// running is `unknown`, and a later call to any of them fails.
    #[non_exhaustive]
    BindingExited {
        bindings: Vec<String>,
        cause: String,
    },
    /// What the session's skill sources hold, as the session started: each
    /// skill served, and why any other was left out, shadowed by an earlier
    /// source's, or served with a warning. Read from each skill's metadata
    /// alone, with no Python run.
    #[non_exhaustive]
    SkillDiscovered {
        skills: Vec<DiscoveredSkill>,
        diagnostics: Vec<SkillDiagnostic>,
    },
    /// A skill's package was imported in the interpreter -- again, after
    /// `outrig.skills.reload`, when `reload` -- by `agent`'s code, or by code
    /// outside every execution when that is `None`. `digest` is its files'
    /// when it was imported; `code` and `reason` say why the import failed,
    /// when it did.
    #[non_exhaustive]
    SkillImported {
        skill: String,
        source: String,
        agent: Option<String>,
        digest: Option<String>,
        reload: bool,
        code: Option<String>,
        reason: Option<String>,
    },
    /// A `/name` the user typed reached the event's subject on its user
    /// channel as message `message`, naming `skill`, which `source` serves.
    /// Nothing of the skill runs until the agent's code invokes it.
    #[non_exhaustive]
    SkillDirectiveReceived {
        skill: String,
        source: String,
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
        invocation: InvocationId,
        skill: String,
        source: String,
        digest: String,
        params: Option<String>,
        by: ParamsBy,
        call: Option<AgentRequestId>,
        bindings: Vec<String>,
    },
    /// The entry of `invocation` ended, `duration` after it was called:
    /// `result` names the type of what it returned and `exception` the type
    /// of what it raised. `usage` is what the model requests of every child
    /// made inside the invocation used, its parser's included, or `None` when
    /// none reported anything.
    #[non_exhaustive]
    SkillInvocationFinished {
        invocation: InvocationId,
        outcome: InvocationEnd,
        #[serde(serialize_with = "seconds")]
        duration: Duration,
        result: Option<String>,
        exception: Option<String>,
        usage: Option<Usage>,
    },
    /// A child agent was launched, by `parent`; the event's subject is the
    /// child's.
    #[non_exhaustive]
    AgentSpawned {
        name: String,
        parent: String,
        kind: ChildKind,
        model: String,
        depth: u32,
        inputs: Vec<InputBrief>,
        channels: Vec<ChannelBrief>,
    },
    /// A request was sent to the event's subject, a child: a submission, when
    /// `channel` is `None`, or a request on that channel. `body` is a bounded
    /// preview of what was sent -- a submission's inputs, by name.
    #[non_exhaustive]
    AgentRequestSent {
        request: AgentRequestId,
        channel: Option<String>,
        sender: String,
        body: String,
        attempts_max: u32,
    },
    /// The child took the request: its code received it from its channel, or
    /// it was given the submission at the start of `round`.
    #[non_exhaustive]
    AgentRequestReceived {
        request: AgentRequestId,
        round: Option<u32>,
    },
    /// The child answered the request, with `value` as a bounded preview. An
    /// answer that failed its check is followed by `agent.request.invalid`.
    #[non_exhaustive]
    AgentRequestReplied {
        request: AgentRequestId,
        attempt: u32,
        value: String,
    },
    #[non_exhaustive]
    AgentRequestInvalid {
        request: AgentRequestId,
        attempt: u32,
        problems: Vec<String>,
    },
    /// A work child reported progress on the submission it works on, which
    /// its sender reads from the handle. Settles nothing.
    #[non_exhaustive]
    AgentRequestProgress {
        request: AgentRequestId,
        body: String,
    },
    /// The child failed the request on purpose, saying why.
    #[non_exhaustive]
    AgentRequestFailed {
        request: AgentRequestId,
        message: String,
    },
    /// Its sender cancelled the request, before or after the child received
    /// it.
    #[non_exhaustive]
    AgentRequestCancelled {
        request: AgentRequestId,
        received: bool,
    },
    /// How the request ended, how many answers it took, and the child's
    /// rounds it spanned. `cause` is how a model call of the child's failed
    /// and ended a round of them, when one did and the request was rejected
    /// for the child's rounds ending without completing: the text its
    /// sender's exception carries as `__cause__`.
    #[non_exhaustive]
    AgentRequestSettled {
        request: AgentRequestId,
        outcome: RequestEnd,
        attempts: u32,
        rounds: Vec<u32>,
        cause: Option<String>,
    },
    /// An answer to a request that had already ended, refused.
    #[non_exhaustive]
    AgentRequestRefused {
        request: AgentRequestId,
        ended: RequestEnd,
    },
    /// The event's subject released `released`; that ended `ended`, every
    /// child in their subtrees, and settled `requests`.
    #[non_exhaustive]
    AgentReleased {
        released: Vec<String>,
        ended: Vec<String>,
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
        request: AgentRequestId,
        parent: String,
        module: String,
        qualname: String,
        digest: Option<String>,
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
        request: AgentRequestId,
        outcome: RequestEnd,
        usage: Option<Usage>,
        invocation: Option<InvocationId>,
        value: Option<String>,
    },
    /// An agent class's instance, made by `parent`'s code, launched its child,
    /// the event's subject: the class's `__module__` and `__qualname__`, the
    /// SHA-256 digest of the source that declared it, or `None` when none
    /// could be read, the child's inputs, and its request channels.
    #[non_exhaustive]
    AgentInstanceStarted {
        instance: AgentInstanceId,
        parent: String,
        module: String,
        qualname: String,
        digest: Option<String>,
        inputs: Vec<InputBrief>,
        channels: Vec<ChannelBrief>,
    },
    /// The instance's child ended its orienting round, the one it has as it
    /// launches, and is ready; or `error` says why it cannot have that round.
    #[non_exhaustive]
    AgentInstanceReady {
        instance: AgentInstanceId,
        error: Option<String>,
    },
    /// The instance released its child, `how`, which settled `requests`.
    /// `usage` is what the child's own model requests used, or `None` when
    /// none reported anything.
    #[non_exhaustive]
    AgentInstanceReleased {
        instance: AgentInstanceId,
        how: InstanceRelease,
        requests: Vec<AgentRequestId>,
        usage: Option<Usage>,
    },
    /// The instance was collected: its release follows, unless the session's
    /// close had released it.
    #[non_exhaustive]
    AgentInstanceCollected { instance: AgentInstanceId },
    /// The handle of `request`, made through the instance, was collected
    /// while the request was open. The request goes on.
    #[non_exhaustive]
    AgentInstanceHandleCollected {
        instance: AgentInstanceId,
        request: AgentRequestId,
    },

    // Integration audit: what crossed to a provider, or across a channel.
    /// A round ended as its model yielded, or as a limit stopped it.
    /// `usage` and `attempts` are the round's own; `descendants` the requests
    /// the children it launched made while it ran, and `total` what the two
    /// used together, each attempt counted once.
    #[non_exhaustive]
    ModelRoundCompleted {
        round: u32,
        stopped: Option<String>,
        usage: Option<Usage>,
        calls: Vec<CallUsage>,
        attempts: Vec<AttemptId>,
        input_tokens_max: Option<u64>,
        descendants: Vec<AttemptId>,
        total: Option<Usage>,
    },
    #[non_exhaustive]
    ModelRoundFailed {
        round: u32,
        error: String,
        calls: Vec<CallUsage>,
        usage: Option<Usage>,
        attempts: Vec<AttemptId>,
        descendants: Vec<AttemptId>,
        total: Option<Usage>,
    },
    #[non_exhaustive]
    ModelRoundDropped {
        round: u32,
        calls: Vec<CallUsage>,
        usage: Option<Usage>,
        attempts: Vec<AttemptId>,
        descendants: Vec<AttemptId>,
        total: Option<Usage>,
    },
    /// Requests a child the round launched made after the round had ended:
    /// spend added to that round's tree once its total was published.
    #[non_exhaustive]
    ModelRoundUsage {
        round: u32,
        attempts: Vec<AttemptId>,
        usage: Option<Usage>,
    },
    /// One request a provider was sent, and how it ended.
    ModelAttempt(ModelAttempt),
    #[non_exhaustive]
    ModelRetry {
        model: String,
        /// The try that failed, counted from 1 within its layer's retries.
        attempt: u32,
        #[serde(serialize_with = "seconds")]
        delay: Duration,
        error: String,
        call_id: CallId,
        attempt_id: AttemptId,
    },
    #[non_exhaustive]
    ModelFailover {
        from: String,
        to: String,
        error: String,
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
        model: String,
        call_id: CallId,
        turn: u64,
        finish_reason: String,
    },
    /// A usage record that arrived after its attempt's event, accepted: it
    /// replaces that attempt's null usage, once.
    #[non_exhaustive]
    ModelUsageReplaced {
        call_id: CallId,
        attempt_id: AttemptId,
        usage: Usage,
    },
    /// A usage record that changed nothing.
    #[non_exhaustive]
    ModelUsageRefused {
        call_id: Option<CallId>,
        attempt_id: AttemptId,
        usage: Usage,
        reason: UsageRefusal,
    },
    #[non_exhaustive]
    MessageSent {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
        body: String,
    },
    #[non_exhaustive]
    MessageRefused {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
        reason: String,
    },
    #[non_exhaustive]
    MessageReceived {
        message: MessageId,
        channel: String,
        from: String,
        to: String,
    },
    /// A request agent code made of a hosted object, as it arrived: published
    /// before anything is decided about it. `by` says who saw it -- the
    /// bindings process, which received it, or, for one it never received,
    /// the owner's relay or the interpreter. `execid` and `invocation` are the
    /// execution and the skill invocation it was made in, as the interpreter
    /// said: diagnostic context, which the agent's own code can change.
    #[non_exhaustive]
    HostedReceived {
        request: RequestId,
        agent: String,
        binding: String,
        execid: Option<ExecId>,
        invocation: Option<InvocationId>,
        operation: Operation,
        member: Option<String>,
        target: Option<String>,
        arguments: Vec<String>,
        keywords: std::collections::BTreeMap<String, String>,
        parent: Option<RequestId>,
        by: ReceivedBy,
    },
    /// A request's target is about to be invoked: published before it is.
    #[non_exhaustive]
    HostedDispatched {
        request: RequestId,
        agent: String,
        binding: String,
    },
    /// How a request ended.
    #[non_exhaustive]
    HostedEnded {
        request: RequestId,
        agent: String,
        binding: String,
        outcome: HostedOutcome,
        reason: Option<HostedReason>,
        detail: Option<String>,
        exception: Option<String>,
        traceback: Option<String>,
        interrupted: bool,
        #[serde(serialize_with = "seconds")]
        duration: Duration,
    },
    /// A hosted library called back a callable agent code passed it, in the
    /// call `parent`.
    #[non_exhaustive]
    HostedCallback {
        parent: RequestId,
        agent: String,
        binding: String,
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
            Payload::ToolResultTruncated { .. } => "tool.result.truncated",
            Payload::ContextPromoted { .. } => "context.promoted",
            Payload::ContextDemoted { .. } => "context.demoted",
            Payload::OutputUnattributed { .. } => "output.unattributed",
            Payload::InterpreterDiagnostic { .. } => "interpreter.diagnostic",
            Payload::InterpreterExited { .. } => "interpreter.exited",
            Payload::BindingRefused { .. } => "binding.refused",
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
            | Self::BindingExited { .. }
            | Self::SkillDiscovered { .. } => None,
            // An import is the agent's whose code made it, which may be none.
            Self::SkillImported { agent, .. } => agent.as_deref().map(Subject::agent),
            Self::HostedReceived { agent, .. }
            | Self::HostedDispatched { agent, .. }
            | Self::HostedEnded { agent, .. }
            | Self::HostedCallback { agent, .. } => Some(Subject::agent(agent)),
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
    Getattr,
    Setattr,
    Delattr,
    Call,
    Callattr,
    Repr,
    Str,
    Cmp,
    Hash,
    Dir,
    Instancecheck,
    Buffiter,
    Oldslicing,
    Ctxexit,
    Ping,
    Close,
    Getroot,
    Inspect,
    Pickle,
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
    /// The bindings process was killed at shutdown.
    Killed,
    /// Its connection closed before the bindings process read it.
    Disconnected,
}

/// A tool a model call carries, as the provider is told of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// One model call's manifest: what it was sent of the conversation, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelCall {
    /// This assembly's place among the session's, from 0. A call moved to
    /// another model is assembled again, under the same `call_id`.
    pub call: u64,
    pub call_id: CallId,
    pub round: u32,
    pub budget: CallBudget,
    pub estimate: u64,
    pub carried: Vec<Chosen>,
    pub evicted: Vec<Chosen>,
    pub withheld: Vec<Chosen>,
    /// The round's opening, as rig's message JSON, when it was the call's
    /// prompt.
    pub opening: Option<Value>,
    pub adjacent: Vec<Repeat>,
    pub left_out: Vec<LeftOut>,
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
    pub model: String,
    pub window: u32,
    pub window_assumed: bool,
    pub reserve: u32,
    pub overhead: u64,
    /// What the call's `estimate` was multiplied by to be held to the window: 1.0
    /// is the estimate as it stands, and above it the model's provider has
    /// reported more input tokens for a request than the estimate said.
    pub estimate_scale: f64,
    pub max_tokens: Option<u32>,
    pub role_alternation: RoleAlternation,
}

/// A turn a call carried or left out, and why it was chosen.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Chosen {
    pub turn: u64,
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
    pub role: Role,
}

/// Whose side of the conversation a message is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    User,
    Assistant,
}

/// A part of a carried turn a call was not sent, by its place: the turn, the
/// message within it, and the part within that.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct LeftOut {
    pub turn: u64,
    pub message: u64,
    pub part: u64,
}

/// A carried turn whose results a provider refused a call for, which the call
/// was sent `notice` in place of each of.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct RefusedTurn {
    pub turn: u64,
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
    Ok,
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
    Blocked,
    Spinning,
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
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
}

/// Output an earlier execution wrote after it reported, delivered with a
/// later one's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Background {
    pub id: ExecId,
    pub output: String,
    pub dropped: u64,
}

/// Tokens, as a provider reported them. Where a provider reported nothing,
/// the field that holds a `Usage` is `None`, never zeros.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[non_exhaustive]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
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
    pub index: u64,
    pub model: String,
    pub call_id: CallId,
    pub attempt_id: AttemptId,
    pub usage: Option<Usage>,
}

/// One request a provider was sent: the call it served, the settings it went
/// with, and how it ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ModelAttempt {
    pub call_id: CallId,
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
    Work,
    Request,
}

/// One of a child's inputs, bound as a variable before its first round: its
/// name, its type, and a bounded preview of its value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct InputBrief {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub preview: String,
}

/// One skill the session serves, as discovery found it: its name, the source
/// serving it, its description, the digest of its files then, and whether it
/// has a `skill.py`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct DiscoveredSkill {
    pub name: String,
    pub source: String,
    pub description: String,
    pub digest: String,
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
    pub skill: Option<String>,
    pub source: String,
    pub code: String,
    pub message: String,
}

/// One of a request child's channels: its name, and the type of a request on
/// it and of the reply.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ChannelBrief {
    pub name: String,
    pub request: String,
    pub reply: String,
}

/// How a request an agent made of a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum RequestEnd {
    /// An answer passed its check, and is its result.
    Accepted,
    /// Its answers failed their checks past the attempt limit, or a work
    /// child's rounds ended without completing it.
    Rejected,
    /// The child failed it on purpose.
    Failed,
    /// Its sender cancelled it.
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
    /// It was collected unreleased.
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
    #[error("no event is waiting")]
    Empty,
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
