//! The session's agents, read at one moment, as its owner reads them.
//!
//! The registry of a session's agents keeps every agent of its tree under one lock, with what
//! each is doing and every request made of a child. A roster is a copy of that, taken in one
//! hold of the lock: it lags any change by that one lock at most, never mixes two moments, and
//! awaits nothing -- not the interpreter, not a kernel busy in code that never yields. No event is
//! consulted, so a subscription that drops events, or none at all, changes nothing in it.

use super::stop::AgentState;

/// The session's agents at one moment ([`Session::roster`](super::Session::roster)).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Roster {
    /// The session's count of changes to its agents' states at the read: every change a
    /// [`Status::seq`](super::Status::seq) numbers this or lower is in it, and none after.
    pub seq: u64,
    /// Each agent, in the order of the tree: an agent before the agents it launched, and those
    /// in the order it launched them. A child whose parent is gone -- a root whose construction
    /// failed after its constructor launched the child, which outlives it while its kernel ends
    /// -- comes after the tree, in launch order, naming the parent no row lists.
    ///
    /// Listed are the primary agent, when the session has one, and every child from its launch
    /// until its kernel is gone -- what `children-max` counts. A released child stays listed,
    /// [`RosterState::Releasing`], until its kernel ends, and one whose kernel does not end,
    /// [`RosterState::Wedged`], until it does. The program that owns the session is no agent,
    /// and is not listed.
    pub agents: Vec<AgentRow>,
}

/// One agent of a [`Roster`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentRow {
    /// The agent's id, which its events name as their subject: `primary`, or `c1`, `c2`, ... in
    /// the order the session launched them.
    pub id: String,
    /// What it is.
    pub kind: AgentKind,
    /// The name its launch gave it -- `runtime.spawn`'s first argument, or a root's class's
    /// name -- or `None` for the primary agent.
    pub name: Option<String>,
    /// What launched it, or `None` for the primary agent.
    pub parent: Option<Parent>,
    /// How deep in the tree it is: 1 for the primary agent and for a root, and one more than its
    /// parent for any other child.
    pub depth: u32,
    /// What it is doing, or `None` for the primary agent, whose rounds its owner drives and whose
    /// every outcome [`Session::round`](super::Session::round) returns.
    pub state: Option<RosterState>,
    /// The latest state at rest it came to, the one it is in included -- what
    /// [`RootHandle::next_stop`](super::RootHandle::next_stop) would have returned last -- or
    /// `None` before it came to one, and for the primary agent.
    pub last_stop: Option<AgentState>,
    /// The `[models.<name>]` row it runs on, as named: an alias stays an alias.
    pub model: String,
    /// The requests made of it that have not ended, or `None` for the primary agent, of which
    /// none is made.
    pub outstanding: Option<Outstanding>,
}

/// What an agent of a [`Roster`] is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentKind {
    /// The session's primary agent.
    Primary,
    /// A root: an agent class instance the program that owns the session constructed.
    #[non_exhaustive]
    Root {
        /// Its class, as `module.qualname`.
        class: String,
    },
    /// A child that answers requests on its channels.
    #[non_exhaustive]
    Request {
        /// Its class, as `module.qualname`, when it is an agent class instance's; `None` for one
        /// `runtime.spawn` launched with `requests=`.
        class: Option<String>,
    },
    /// A child that takes submissions and completes each: one `runtime.spawn` launched without
    /// `requests=`, or a typed call's.
    Work,
}

/// What launched an agent of a [`Roster`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Parent {
    /// The program that owns the session: the agent is a root.
    Owner,
    /// The agent with this id, whose code launched it.
    Agent(String),
}

/// What a child of a [`Roster`] is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RosterState {
    /// What its stop read says ([`RootHandle::status`](super::RootHandle::status) for a root):
    /// a round open, or why it is at rest.
    Agent(AgentState),
    /// It was released -- by its parent, its owner, or the session's close -- and its kernel has
    /// not ended yet. It counts against `children-max` until it has.
    Releasing,
    /// It was released, and its kernel did not end within two seconds: its code never yields.
    /// It counts against `children-max` until the kernel ends.
    Wedged,
}

/// The requests made of a child that have not ended, by where each is: every one the session
/// holds for it, a work child's waiting submissions included.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Outstanding {
    /// Requests on its channels it has not taken.
    pub unread: usize,
    /// Submissions waiting their turn behind the one it works on.
    pub queued: usize,
    /// Requests it took, and submissions it was given, that it has not answered.
    pub taken: usize,
}
