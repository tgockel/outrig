//! Subagents: extra agent loops over the session's own container and tools.
//!
//! A subagent is a **headless REPL**. The REPL feeds
//! [`run_turn`](crate::llm::RigAgent::run_turn) lines from stdin and keeps a
//! `Vec<Message>` across them; a subagent feeds it prompts from the parent
//! agent and keeps a `Vec<Message>` the same way. Same loop, same history
//! handling, different driver -- so this module is mostly bookkeeping around
//! machinery that already exists.
//!
//! Nothing here starts a container or connects an MCP server. A subagent
//! borrows the parent's `Arc<McpClient>` set, which is why launching one is
//! nearly free and why the trust-model invariant that "the agent cannot grow
//! its own environment" still holds.
//!
//! A subagent may itself launch subagents, bounded by `subagent_depth_max`
//! (the primary is the root at depth 1). Each launching agent gets its own
//! registry, so it only ever sees the subagents it launched; the depth check in
//! [`build_subagent_agent`] withholds the launch tools once the limit is
//! reached. Shutdown and release therefore recurse through the whole tree.
//!
//! Task ownership deliberately does *not* run through the name map. A registry
//! keeps a ledger of every task it has spawned ([`Spawned`]), and
//! [`SubagentRegistry::shutdown`] joins that; the map keyed by name holds only
//! what the parent agent interacts with. Freeing a name therefore cannot
//! strand a task with the session's tool clones still in it -- which is the
//! whole reason shutdown can promise anything to teardown.
//!
//! Coordination is on *results*, not run state: see [`state`] for the inbox
//! and the readable predicate the parent blocks on.

pub mod injection;
pub mod state;
mod transcript;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures_util::future::select_all;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::llm::ResolvedAgent;
use crate::session_tool::SessionTool;
use state::{Outcome, SubagentShared};

/// Longest subagent name accepted. Names are handles the model types back, so
/// they stay short enough to be cheap to repeat.
const MAX_NAME_LEN: usize = 40;

/// How long [`SubagentRegistry::shutdown`] waits for aborted tasks before
/// giving up and letting teardown proceed.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Everything needed to build a subagent's agent loop, cloned from the
/// session. Held by the registry so a launch needs only a name and a prompt.
#[derive(Clone)]
pub struct SubagentContext {
    /// The session's resolved agent. A subagent reuses its model, provider and
    /// limits; only the preamble is replaced, by whatever the parent passes.
    pub resolved: ResolvedAgent,
    /// The session's MCP-backed tools. Cloning shares the live connections.
    pub mcp_tools: Vec<SessionTool>,
    pub cache_root: PathBuf,
    /// Where per-subagent transcripts go, alongside `<server>.stderr`.
    pub log_dir: PathBuf,
    /// The depth of the subagents *this* registry launches. The primary agent
    /// is the root at depth 1, so the session's registry launches at depth 2. A
    /// subagent at depth `D` may launch its own children (at `D + 1`) only while
    /// `D < resolved.subagent_depth_max`.
    pub depth: u32,
    /// Shared with the REPL agent rather than owned: a per-subagent registry
    /// would re-load the model's weights for every launch.
    #[cfg(feature = "local-llm")]
    pub registry: Arc<crate::llm::LlmRegistry>,
}

/// One live subagent as the *parent* sees it: where to read its results, how
/// to prompt it, how to stop it. Notably not where its task is owned -- see
/// [`Spawned`].
struct Entry {
    shared: Arc<SubagentShared>,
    prompts: mpsc::UnboundedSender<String>,
    /// Cancellation only. The [`JoinHandle`] lives in the ledger so that
    /// freeing this name cannot detach the task.
    abort: tokio::task::AbortHandle,
    /// The parent's read position. Deliberately here rather than on
    /// [`SubagentShared`]: it describes the *reader*, not the subagent.
    watermark: u64,
    /// This subagent's own registry, present only when it was given launch
    /// tools (i.e. its depth was under the max). An `Arc` clone of the one the
    /// ledger holds, kept here so [`SubagentRegistry::release`] can cancel this
    /// subagent's descendants without a search.
    child: Option<Arc<SubagentRegistry>>,
}

/// One task this registry spawned, owned independently of the name it was
/// launched under.
///
/// [`SubagentRegistry::shutdown`] joins these, which is why a subagent whose
/// name was released -- or that lost the name race and was never registered at
/// all -- is still reaped before teardown. Were the [`JoinHandle`] owned by
/// [`Entry`] instead, every path that frees a name would have to remember to
/// hand the task back, and one that forgot would drop an aborted-but-unreaped
/// task still holding the session's `Arc<McpClient>` clones.
struct Spawned {
    task: JoinHandle<()>,
    /// The registry this subagent launches through, when depth allowed it one.
    /// Held here, not only on [`Entry`], so the descendant tree stays reachable
    /// after the name goes away.
    child: Option<Arc<SubagentRegistry>>,
}

/// The live subagents of one session.
///
/// Lives on the REPL session rather than inside the prompt future, because
/// Ctrl-C drops that future and subagents are specified to survive it.
pub struct SubagentRegistry {
    ctx: SubagentContext,
    entries: Mutex<BTreeMap<String, Entry>>,
    /// Every task launched here that has not been swept as finished. The two
    /// locks are never held at once, so their order is not a hazard.
    spawned: Mutex<Vec<Spawned>>,
}

impl SubagentRegistry {
    pub fn new(ctx: SubagentContext) -> Self {
        Self {
            ctx,
            entries: Mutex::new(BTreeMap::new()),
            spawned: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries.lock().expect("subagent registry poisoned")
    }

    fn spawned_lock(&self) -> std::sync::MutexGuard<'_, Vec<Spawned>> {
        self.spawned.lock().expect("subagent registry poisoned")
    }

    /// Launch a subagent under `name`, running `prompt` as its first round.
    pub async fn launch(
        &self,
        name: &str,
        preamble: Option<String>,
        prompt: String,
    ) -> Result<(), String> {
        validate_name(name)?;
        {
            let entries = self.lock();
            if entries.contains_key(name) {
                return Err(format!(
                    "subagent {name:?} is already live; release it first or pick another name"
                ));
            }
            if entries.len() >= self.ctx.resolved.subagent_width_max as usize {
                return Err(width_limit_error(self.ctx.resolved.subagent_width_max));
            }
        }
        // Each launch pays for the previous ones' bookkeeping, so a session
        // that cycles handles does not carry a record per launch for its whole
        // life. A swept record's task is already reaped and holds nothing.
        self.spawned_lock().retain(|s| !s.tree_finished());

        let shared = Arc::new(SubagentShared::new());
        let (agent, child) = build_subagent_agent(&self.ctx, &shared, name, preamble).await?;

        let (prompts, rx) = mpsc::unbounded_channel();
        prompts
            .send(prompt)
            .map_err(|_| "subagent channel closed before its first round".to_string())?;

        let task = tokio::spawn(run_rounds(
            name.to_string(),
            agent,
            shared.clone(),
            rx,
            self.ctx.log_dir.clone(),
        ));
        let abort = task.abort_handle();
        // Register before the name check below, not after it: the ledger is
        // what makes shutdown's guarantee hold no matter which way the rest of
        // this function exits.
        self.spawned_lock().push(Spawned {
            task,
            child: child.clone(),
        });

        // Re-check under the lock: an await happened above, so another launch
        // could have taken the name in the meantime.
        let mut entries = self.lock();
        if entries.contains_key(name) {
            // The ledger still owns the handle, so shutdown will join this
            // task rather than leave it detached with its tool clones.
            abort.abort();
            return Err(format!("subagent {name:?} is already live"));
        }
        if entries.len() >= self.ctx.resolved.subagent_width_max as usize {
            // As with the duplicate-name race, the ledger owns this task and
            // will reap it at shutdown; abort it now so it cannot run after
            // launch refuses it.
            abort.abort();
            return Err(width_limit_error(self.ctx.resolved.subagent_width_max));
        }
        entries.insert(
            name.to_string(),
            Entry {
                shared,
                prompts,
                abort,
                watermark: 0,
                child,
            },
        );
        Ok(())
    }

    /// Deliver a prompt whether the subagent is idle or running. Idle starts a
    /// new round; running injects into the round in flight.
    ///
    /// Run state is never a precondition here, because the tool surface gives
    /// the parent no non-blocking way to observe it.
    pub fn send(&self, name: &str, prompt: String) -> Result<&'static str, String> {
        let entries = self.lock();
        let entry = entries
            .get(name)
            .ok_or_else(|| unknown_name(name, &entries))?;
        match entry.shared.snapshot().state {
            state::RunState::Running => {
                entry.shared.queue_injection(prompt);
                Ok("injected into the round in flight")
            }
            state::RunState::Idle { .. } => {
                entry
                    .prompts
                    .send(prompt)
                    .map_err(|_| format!("subagent {name:?} is no longer running"))?;
                Ok("started a new round")
            }
        }
    }

    /// Block until at least `min_count` of `names` are readable, then report
    /// which. Carries no payloads and moves no watermark: results can be
    /// large, and the parent decides how much of one enters its context by
    /// calling [`Self::get_result`] per subagent.
    pub async fn wait_results(
        &self,
        names: &[String],
        min_count: usize,
    ) -> Result<Vec<String>, String> {
        if names.is_empty() {
            return Err("wait_results needs at least one name".to_string());
        }
        let mut receivers = Vec::with_capacity(names.len());
        {
            let entries = self.lock();
            for name in names {
                let entry = entries
                    .get(name)
                    .ok_or_else(|| unknown_name(name, &entries))?;
                receivers.push(entry.shared.subscribe());
            }
        }

        loop {
            let ready = self.readable_among(names)?;
            if ready.len() >= min_count {
                return Ok(ready);
            }
            // Re-derive the wakeups each pass: the borrows end with the await,
            // and a `watch` receiver never misses a change it was subscribed
            // for, so re-arming cannot drop an update.
            let waits = receivers.iter_mut().map(|rx| Box::pin(rx.changed()));
            let (result, _, _) = select_all(waits).await;
            if result.is_err() {
                // A sender dropped: that subagent's task is gone, so state can
                // no longer change. One more pass reports whatever is readable
                // rather than hanging.
                return self.readable_among(names);
            }
        }
    }

    fn readable_among(&self, names: &[String]) -> Result<Vec<String>, String> {
        let entries = self.lock();
        let mut ready = Vec::new();
        for name in names {
            let entry = entries
                .get(name)
                .ok_or_else(|| unknown_name(name, &entries))?;
            if entry.shared.snapshot().readable(entry.watermark) {
                ready.push(name.clone());
            }
        }
        Ok(ready)
    }

    /// Block until this subagent is readable, return its outcome, and advance
    /// the watermark past it. The only consumer of a version.
    pub async fn get_result(&self, name: &str) -> Result<Outcome, String> {
        let (shared, mut rx) = {
            let entries = self.lock();
            let entry = entries
                .get(name)
                .ok_or_else(|| unknown_name(name, &entries))?;
            (entry.shared.clone(), entry.shared.subscribe())
        };

        loop {
            let snapshot = shared.snapshot();
            // Read and advance under one lock. Splitting them let two
            // concurrent reads of the same subagent observe the same watermark
            // and both return the outcome, breaking "each result is delivered
            // once".
            let claimed = {
                let mut entries = self.lock();
                let entry = entries.get_mut(name).ok_or_else(|| {
                    format!("subagent {name:?} was released while waiting for its result")
                })?;
                match snapshot.read(entry.watermark) {
                    Some(outcome) => {
                        // Only a genuinely newer version advances the read
                        // position; the idle-without-publishing case is
                        // level-triggered and must stay readable.
                        if snapshot.version > entry.watermark {
                            entry.watermark = snapshot.version;
                        }
                        Some(outcome)
                    }
                    None => None,
                }
            };
            if let Some(outcome) = claimed {
                return Ok(outcome);
            }
            if rx.changed().await.is_err() {
                return Err(format!("subagent {name:?} stopped without a result"));
            }
        }
    }

    /// End these subagents and free their names. A relaunch under a freed name
    /// starts a fresh inbox at version 0.
    ///
    /// Descendants are aborted too: a released subagent's own subagents would
    /// otherwise keep running, detached, still calling tools into the session.
    /// This is best-effort cancellation (no awaiting) -- the session is live and
    /// still needs its MCP children, so the awaited reap is left to
    /// [`Self::shutdown`] at teardown. That hand-off is structural rather than
    /// a convention this function has to honor: the tasks stay in the ledger,
    /// which only [`Self::shutdown`] drains, so freeing the names here cannot
    /// put them out of its reach.
    pub fn release(&self, names: &[String]) -> Result<Vec<String>, String> {
        let mut entries = self.lock();

        // Resolve the whole request before changing the registry. Returning an
        // error after releasing an earlier name would tell the caller a state
        // that no longer exists, making the failed call unsafe to recover from.
        let mut seen = BTreeSet::new();
        for name in names {
            if !seen.insert(name) {
                return Err(format!(
                    "subagent {name:?} was named more than once in this release request"
                ));
            }
            entries
                .get(name)
                .ok_or_else(|| unknown_name(name, &entries))?;
        }

        let mut released = Vec::with_capacity(names.len());
        for name in names {
            // The preflight above proves the name is live. A repeated name is
            // invalid after its first removal, so reject duplicates before
            // this loop rather than panic or partially release below.
            let entry = entries
                .remove(name)
                .expect("release names were resolved before mutation");
            entry.abort_tree();
            released.push(name.clone());
        }
        Ok(released)
    }

    /// Stop every subagent -- and every subagent *they* launched -- and wait for
    /// all of those tasks to actually end.
    ///
    /// The waiting is the point. Aborting only schedules cancellation; until
    /// the runtime reaps the task it still owns its agent, and therefore clones
    /// of the session's `Arc<McpClient>`. Returning before that happens means
    /// `teardown`'s `Arc::try_unwrap` finds outstanding refs, skips the
    /// graceful MCP shutdown, and leaves the `podman exec` children running --
    /// which is what kept the process alive after Ctrl-C.
    ///
    /// "Every subagent" means every task, not every live handle. A nested
    /// subagent holds those clones just as a direct one does, and so does one
    /// the parent released a moment ago, so the reap walks the ledger -- which
    /// outlives names -- through the whole tree.
    pub async fn shutdown(&self) {
        // One grace budget for the entire tree: a wedged subagent anywhere
        // delays exit rather than preventing it.
        if tokio::time::timeout(SHUTDOWN_GRACE, self.shutdown_tree())
            .await
            .is_err()
        {
            tracing::warn!(
                target: "outrig::subagent",
                "subagent tasks did not stop within {SHUTDOWN_GRACE:?}; \
                 continuing teardown"
            );
        }
    }

    /// Recursively abort every task this registry spawned and all their
    /// descendants, waiting for each to be reaped. Descendants are drained
    /// before this level's tasks are joined, so tool clones release bottom-up:
    /// the deepest subagent must be gone before teardown's `Arc::try_unwrap`
    /// at the top.
    ///
    /// The ledger, not the name map, is what gets drained -- a subagent
    /// released mid-session is exactly as much of a teardown problem as a live
    /// one, and by then it has no name.
    fn shutdown_tree(&self) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            // Names go first: nothing can be launched into a registry being
            // torn down, and a read of a gone subagent should say so. Dropping
            // the entries is safe for the recursion below, because each one's
            // `child` is only an `Arc` clone of the ledger's.
            self.lock().clear();
            let spawned = std::mem::take(&mut *self.spawned_lock());
            // Abort every task at this level up front so the whole layer is
            // cancelling in parallel while we drain the layers beneath it.
            // Aborting an already-aborted or finished task is a no-op.
            for entry in &spawned {
                entry.task.abort();
            }
            let mut handles = Vec::with_capacity(spawned.len());
            for entry in spawned {
                if let Some(child) = &entry.child {
                    child.shutdown_tree().await;
                }
                handles.push(entry.task);
            }
            // An aborted task ends at its next await point, so joining is
            // normally immediate once its children are gone.
            if !handles.is_empty() {
                futures_util::future::join_all(handles).await;
            }
        })
    }

    /// Abort every live subagent and its descendants without awaiting. The
    /// recursive step under [`Self::release`].
    fn abort_all(&self) {
        for entry in self.lock().values() {
            entry.abort_tree();
        }
    }

    /// Whether every task spawned here, at any depth, has been reaped -- and so
    /// holds no tool clones and can be forgotten. The recursion into child
    /// registries is what keeps [`Self::launch`]'s sweep honest: a subagent
    /// whose grandchild is still cancelling has to stay in the ledger.
    fn tree_finished(&self) -> bool {
        self.spawned_lock().iter().all(Spawned::tree_finished)
    }
}

impl Entry {
    /// Abort this subagent and, recursively, every descendant it launched.
    /// Sync and best-effort: aborting only schedules cancellation, which is
    /// enough mid-session -- the awaited reap belongs to
    /// [`SubagentRegistry::shutdown`] at teardown.
    fn abort_tree(&self) {
        if let Some(child) = &self.child {
            child.abort_all();
        }
        self.abort.abort();
    }
}

impl Spawned {
    /// `is_finished` is true only once the task has completed or been
    /// cancelled, which means its future -- and every tool clone in it -- has
    /// already been dropped.
    fn tree_finished(&self) -> bool {
        self.task.is_finished()
            && self
                .child
                .as_ref()
                .is_none_or(|child| child.tree_finished())
    }
}

/// Build the agent loop one subagent runs, and, when depth allows, the registry
/// it launches its own subagents through.
///
/// It reuses the session's model, provider and limits; only the preamble is
/// replaced. The tool list is always the session's MCP tools plus this
/// subagent's own `outrig__set_result`. When the subagent's depth is under
/// `subagent_depth_max`, it also gets its own [`SubagentRegistry`] and the
/// parent-side launch tools, so it can launch children of its own; at the max
/// depth those are withheld and the returned registry is `None`. Recursion is
/// bounded by that depth check rather than impossible by construction.
async fn build_subagent_agent(
    ctx: &SubagentContext,
    shared: &Arc<SubagentShared>,
    name: &str,
    preamble: Option<String>,
) -> Result<(crate::llm::RigAgent, Option<Arc<SubagentRegistry>>), String> {
    let mut resolved = ctx.resolved.clone();
    resolved.preamble = compose_preamble(preamble.as_deref());

    let mut tools = ctx.mcp_tools.clone();
    tools.push(SessionTool::new(crate::builtin_tool::SetResultTool::new(
        shared.clone(),
        name,
        &ctx.resolved.agent_name,
        ctx.resolved.max_tokens,
    )));

    // This subagent lives at `ctx.depth`. If that is under the max, hand it its
    // own registry and launch tools so it can launch children at `depth + 1`.
    // The recursion is lazy: grandchildren are only built when this subagent
    // actually calls a launch tool, which re-enters here one level deeper, and
    // the depth gate terminates it.
    let child = if ctx.depth < ctx.resolved.subagent_depth_max {
        let mut child_ctx = ctx.clone();
        child_ctx.depth = ctx.depth + 1;
        let child = Arc::new(SubagentRegistry::new(child_ctx));
        tools.extend(crate::builtin_tool::parent_tools(
            child.clone(),
            ctx.resolved.tool_result_max_bytes,
        ));
        Some(child)
    } else {
        None
    };

    let agent = crate::llm::build_agent(
        &resolved,
        tools,
        &ctx.cache_root,
        #[cfg(feature = "local-llm")]
        &ctx.registry,
    )
    .await
    .map_err(|e| format!("could not build subagent: {e}"))?;
    Ok((agent, child))
}

/// The subagent's system prompt: the parent's text, if it passed any, over a
/// fixed fragment about `outrig__set_result`.
///
/// The fragment is not optional. A subagent that has not been told to publish
/// will not, and every round would end in the "stopped without publishing"
/// error. The session's own preamble is deliberately *not* inherited -- the
/// parent decides what context is relevant, including none at all.
fn compose_preamble(parent: Option<&str>) -> String {
    let mut out = String::from(
        "You are a subagent launched by another agent. Report by calling \
         `outrig__set_result` with `status` of \"result\" or \"error\", and the \
         whole report in `body` -- that body is all the agent that launched you \
         will see. Finishing without calling it is reported to that agent as a \
         failure.",
    );
    if let Some(parent) = parent {
        let parent = parent.trim();
        if !parent.is_empty() {
            out.push_str("\n\n");
            out.push_str(parent);
        }
    }
    out
}

/// One subagent's lifetime: a round per prompt the parent sends, with history
/// carried across them. This is the headless-REPL loop -- `run_turn_captured`
/// is the same call the REPL makes for a typed line.
async fn run_rounds(
    name: String,
    agent: crate::llm::RigAgent,
    shared: Arc<SubagentShared>,
    mut prompts: mpsc::UnboundedReceiver<String>,
    log_dir: PathBuf,
) {
    let mut history = Vec::new();
    let mut log = transcript::Transcript::open(&log_dir, &name).await;

    while let Some(prompt) = prompts.recv().await {
        shared.begin_round();
        log.record_prompt(&prompt).await;

        // Rebuilt per model call because the patch rig applies is non-sticky.
        // That means O(queued steers) allocations per completion call, which
        // is fine: a parent redirecting a subagent sends a handful at most.
        let injections = {
            let shared = shared.clone();
            Arc::new(move || {
                shared
                    .injections()
                    .iter()
                    .map(|text| injection::steer_message(text))
                    .collect()
            }) as crate::llm::InjectionSource
        };

        let outcome = agent
            .run_turn_captured(&prompt, &mut history, &name, injections)
            .await;

        // Rig never persisted the steers -- the patch it applied was per-turn
        // and non-sticky -- so fold them in here or the next round will not
        // remember being steered.
        history.extend(
            shared
                .take_injections()
                .iter()
                .map(|text| injection::steer_message(text)),
        );

        match outcome {
            Ok(end) => {
                log.record_reply(&end.reply).await;
                // Recorded before `end_round` so that a round which published
                // nothing can tell the parent *why* it stopped instead of only
                // that it did. Does not displace a truncated report: running
                // out of tool calls is what follows from a report that would
                // not fit.
                if let Some(reason) = end.stopped_early {
                    shared.note_ended_early(reason);
                }
            }
            Err(e) => {
                // Publish only; the transcript entry comes from the outcome
                // block below, which would otherwise record this twice.
                let message = format!("round failed: {e}");
                eprintln!("[outrig]   [{name}] {message}");
                shared.publish(Outcome::Error(message));
            }
        }
        shared.end_round();

        // The published outcome is the part worth keeping: the reply above is
        // whatever the model happened to close with, while this is what the
        // parent actually reads.
        let snapshot = shared.snapshot();
        if snapshot.state.ended_without_publishing() {
            log.record_outcome("no result", "round ended without outrig__set_result")
                .await;
        } else {
            match snapshot.outcome {
                Some(Outcome::Result(text)) => log.record_outcome("result", &text).await,
                Some(Outcome::Error(text)) => log.record_outcome("error", &text).await,
                None => {}
            }
        }
    }
}

fn width_limit_error(limit: u32) -> String {
    format!(
        "subagent width limit of {limit} reached; collect a result and release a subagent with \
         outrig__subagent_release before launching another"
    )
}

fn unknown_name(name: &str, entries: &BTreeMap<String, Entry>) -> String {
    if entries.is_empty() {
        return format!("no subagent named {name:?}; none are running");
    }
    let live: Vec<&str> = entries.keys().map(String::as_str).collect();
    format!("no subagent named {name:?}; live: {}", live.join(", "))
}

/// Names are handles the model repeats back, so they are constrained to short
/// kebab-case: unambiguous to type, and stable to quote in prose.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("subagent name must not be empty".to_string());
    }
    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "subagent name {name:?} is longer than {MAX_NAME_LEN} characters"
        ));
    }
    let shaped = name.split('-').all(|part| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    });
    if !shaped {
        return Err(format!(
            "subagent name {name:?} must be short kebab-case, e.g. \"audit-config\""
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ResolvedProvider;
    use std::time::Duration;

    /// A registry whose subagents talk to a closed port, so every round fails
    /// fast and deterministically. That is enough to exercise the bookkeeping
    /// -- launch, the inbox, watermarks, release -- without a live model.
    ///
    /// The tests below run with `start_paused`, because a connection error is
    /// retried with exponential backoff (`retry::MAX_RETRIES`); real time would
    /// make each round take seconds. Paused time auto-advances while the
    /// subagent sleeps, so the failure surfaces immediately.
    fn test_registry() -> (SubagentRegistry, tempfile::TempDir) {
        test_registry_at(2, outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX)
    }

    /// Like [`test_registry`] but with an explicit launch depth and depth limit,
    /// so a test can place its subagents just under or right at the ceiling.
    fn test_registry_at(
        depth: u32,
        subagent_depth_max: u32,
    ) -> (SubagentRegistry, tempfile::TempDir) {
        let log_dir = tempfile::tempdir().expect("tempdir");
        let resolved = ResolvedAgent {
            agent_name: "primary".to_string(),
            model_name: "m".to_string(),
            model_identifier: "m".to_string(),
            provider_name: "p".to_string(),
            provider: ResolvedProvider::OpenAi {
                // Discard port: connects are refused immediately.
                base_url: "http://127.0.0.1:9".to_string(),
                api_key: "test-key".to_string(),
                request_timeout_secs: Some(1),
            },
            model_weights: None,
            preamble: "session preamble".to_string(),
            temperature: None,
            max_tokens: None,
            tool_call_max: 4,
            tool_result_max_bytes: 4096,
            subagent_depth_max,
            subagent_width_max: outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX,
            image: None,
        };
        let registry = SubagentRegistry::new(SubagentContext {
            resolved,
            mcp_tools: Vec::new(),
            cache_root: PathBuf::from("."),
            log_dir: log_dir.path().to_path_buf(),
            depth,
            #[cfg(feature = "local-llm")]
            registry: Arc::new(crate::llm::LlmRegistry::new()),
        });
        (registry, log_dir)
    }

    /// A stand-in for the session's MCP tools that counts how many copies are
    /// alive, so a test can *observe* release rather than infer it. Every live
    /// subagent task owns a clone of the session's tool list, which is what
    /// teardown's `Arc::try_unwrap` needs released before it can shut the MCP
    /// children down.
    ///
    /// Returns the counter seeded at 1 for the copy the caller installs on the
    /// registry; a test reaches 0 only after dropping the registry too.
    fn counted_tools() -> (Arc<std::sync::atomic::AtomicUsize>, Vec<SessionTool>) {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountedTool(Arc<AtomicUsize>);
        impl Drop for CountedTool {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        impl rig::tool::ToolDyn for CountedTool {
            fn name(&self) -> String {
                "counted".to_string()
            }
            fn description(&self) -> String {
                String::new()
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({"type": "object"})
            }
            fn call<'a>(
                &'a self,
                _args: String,
            ) -> rig::wasm_compat::WasmBoxedFuture<'a, Result<String, rig::tool::ToolError>>
            {
                Box::pin(async { Ok(String::new()) })
            }
        }

        let live = Arc::new(AtomicUsize::new(1));
        let tools = vec![SessionTool::new(CountedTool(live.clone()))];
        (live, tools)
    }

    /// How many copies of a [`counted_tools`] tool are still alive. Zero means
    /// every clone has been dropped.
    fn alive(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Launch `mid`, then reach into the registry it was given and launch a
    /// grandchild `deep` through it, handing back that child registry.
    ///
    /// The nesting is arranged by hand rather than by the subagent: its round's
    /// model call fails against the discard port, so it never gets as far as
    /// calling a launch tool itself. Requires a registry whose depth is under
    /// its limit, or `mid` is a leaf and has no registry to launch through.
    async fn launch_nested(registry: &SubagentRegistry) -> Arc<SubagentRegistry> {
        registry
            .launch("mid", None, "work".to_string())
            .await
            .expect("launch");
        let child = registry
            .lock()
            .get("mid")
            .expect("live")
            .child
            .clone()
            .expect("a subagent below the depth limit gets a registry");
        child
            .launch("deep", None, "work".to_string())
            .await
            .expect("grandchild launch");
        child
    }

    /// Exercises the whole path: launch spawns a round, the round fails, the
    /// driver publishes the failure, and the parent collects it.
    #[tokio::test(start_paused = true)]
    async fn a_failed_round_reaches_the_parent_as_an_error() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "check the config".to_string())
            .await
            .expect("launch succeeds -- the client is built offline");

        let outcome = registry.get_result("audit").await.expect("readable");
        assert!(
            matches!(outcome, Outcome::Error(_)),
            "unreachable provider should surface as an error, got: {outcome:?}"
        );
    }

    /// The watermark is what makes reads edge-triggered. Once a result is
    /// collected, a second read must wait for something genuinely new rather
    /// than handing back the same value.
    #[tokio::test(start_paused = true)]
    async fn a_second_read_blocks_until_there_is_something_new() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "check the config".to_string())
            .await
            .expect("launch succeeds");
        registry.get_result("audit").await.expect("first read");

        let again = tokio::time::timeout(Duration::from_secs(30), registry.get_result("audit"));
        assert!(
            again.await.is_err(),
            "a second read with nothing new must block, not repeat the last value"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_results_returns_early_at_min_count() {
        let (registry, _log_dir) = test_registry();
        for name in ["audit-a", "audit-b"] {
            registry
                .launch(name, None, "work".to_string())
                .await
                .expect("launch succeeds");
        }

        let names = vec!["audit-a".to_string(), "audit-b".to_string()];
        let ready = registry.wait_results(&names, 1).await.expect("wait");
        assert!(
            !ready.is_empty() && ready.len() <= 2,
            "min_count 1 should return as soon as one is ready, got: {ready:?}"
        );
        for name in &ready {
            assert!(names.contains(name));
        }
    }

    /// wait_results reports readiness without collecting, so the result is
    /// still there for get_result afterwards.
    #[tokio::test(start_paused = true)]
    async fn wait_results_does_not_consume_the_result() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("launch succeeds");

        let names = vec!["audit".to_string()];
        registry.wait_results(&names, 1).await.expect("wait");
        registry
            .get_result("audit")
            .await
            .expect("still collectable after waiting on it");
    }

    #[tokio::test(start_paused = true)]
    async fn width_limit_counts_idle_handles_and_release_frees_a_slot() {
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.resolved.subagent_width_max = 1;

        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("first launch");
        registry.get_result("audit").await.expect("collect result");

        let err = registry
            .launch("second", None, "more work".to_string())
            .await
            .expect_err("an idle but unreleased handle still consumes the slot");
        assert!(err.contains("width limit of 1"), "got: {err}");
        assert!(err.contains("outrig__subagent_release"), "got: {err}");

        registry.release(&["audit".to_string()]).expect("release");
        registry
            .launch("second", None, "more work".to_string())
            .await
            .expect("release must free a width slot");
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_name_cannot_be_reused() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("first launch");

        let err = registry
            .launch("audit", None, "other work".to_string())
            .await
            .expect_err("second launch on a live name");
        assert!(err.contains("already live"), "got: {err}");
    }

    /// Releasing frees the handle, and the relaunch starts a fresh inbox at
    /// version 0 rather than inheriting the old one's read position.
    #[tokio::test(start_paused = true)]
    async fn release_frees_the_name_and_resets_the_inbox() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("launch");
        registry.get_result("audit").await.expect("collect");
        registry
            .release(&["audit".to_string()])
            .expect("release succeeds");

        registry
            .launch("audit", None, "work again".to_string())
            .await
            .expect("relaunch under the freed name");
        registry
            .get_result("audit")
            .await
            .expect("fresh inbox is readable again from version 0");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_release_leaves_the_valid_subagent_live_and_collectable() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit-a", None, "work".to_string())
            .await
            .expect("launch");

        let err = registry
            .release(&["audit-a".to_string(), "typo".to_string()])
            .expect_err("an unknown name rejects the whole release");
        assert!(err.contains("typo"), "got: {err}");
        assert!(
            err.contains("audit-a"),
            "the diagnostic must still list the live valid name: {err}"
        );

        registry
            .send("audit-a", "more".to_string())
            .expect("the valid name remains addressable after rejection");
        registry
            .get_result("audit-a")
            .await
            .expect("the valid name remains live and collectable after rejection");
    }

    #[tokio::test(start_paused = true)]
    async fn a_duplicate_release_name_is_rejected_without_releasing_it() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("launch");

        let err = registry
            .release(&["audit".to_string(), "audit".to_string()])
            .expect_err("duplicate names must not make release partially succeed");
        assert!(err.contains("more than once"), "got: {err}");
        registry
            .get_result("audit")
            .await
            .expect("the duplicate request must leave the subagent live");
    }

    #[tokio::test(start_paused = true)]
    async fn unknown_names_name_the_live_ones() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("launch");

        let err = registry.get_result("nope").await.expect_err("unknown name");
        assert!(err.contains("nope") && err.contains("audit"), "got: {err}");

        let err = registry
            .send("nope", "hi".to_string())
            .expect_err("unknown");
        assert!(err.contains("audit"), "got: {err}");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_clears_every_subagent() {
        let (registry, _log_dir) = test_registry();
        for name in ["audit-a", "audit-b"] {
            registry
                .launch(name, None, "work".to_string())
                .await
                .expect("launch");
        }
        registry.shutdown().await;

        let err = registry
            .get_result("audit-a")
            .await
            .expect_err("nothing survives teardown");
        assert!(err.contains("no subagent"), "got: {err}");
    }

    /// The Ctrl-C hang: a subagent task owns clones of the session's MCP
    /// tools, so teardown cannot shut the MCP children down until the task is
    /// actually reaped. Aborting without waiting left them alive and the
    /// process running.
    #[tokio::test]
    async fn shutdown_releases_the_tool_clones_subagents_hold() {
        let (live, tools) = counted_tools();
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.mcp_tools = tools;

        registry
            .launch("audit", None, "work".to_string())
            .await
            .expect("launch");
        registry.shutdown().await;
        drop(registry);

        assert_eq!(
            alive(&live),
            0,
            "every tool clone must be released once subagents are shut down, or \
             teardown cannot close the MCP children"
        );
    }

    /// A child registry has its own width budget: filling the parent's registry
    /// does not consume slots in the child registry.
    #[tokio::test(start_paused = true)]
    async fn width_limit_is_scoped_to_each_registry() {
        let (mut registry, _log_dir) = test_registry_at(2, 3);
        registry.ctx.resolved.subagent_width_max = 1;

        registry
            .launch("mid", None, "work".to_string())
            .await
            .expect("the root registry's only slot");
        let child = registry
            .lock()
            .get("mid")
            .expect("live parent entry")
            .child
            .clone()
            .expect("mid is below the depth limit");

        // The root is at its cap, but the child registry starts empty and has
        // its own cap, so it can launch independently.
        child
            .launch("deep", None, "work".to_string())
            .await
            .expect("the child registry has an independent width slot");

        registry.shutdown().await;
    }

    /// A subagent under the depth limit is handed its own registry (and, with
    /// it, the launch tools); one at the limit is a leaf.
    #[tokio::test(start_paused = true)]
    async fn a_child_registry_is_handed_out_only_below_the_depth_limit() {
        // depth 2 with max 3: 2 < 3, so the subagent may launch its own.
        let (below, _log_a) = test_registry_at(2, 3);
        below
            .launch("mid", None, "work".to_string())
            .await
            .expect("launch");
        assert!(
            below.lock().get("mid").expect("live").child.is_some(),
            "a subagent below the limit should get a registry to launch with"
        );

        // depth 3 with max 3: 3 == 3, so the subagent is a leaf.
        let (at_limit, _log_b) = test_registry_at(3, 3);
        at_limit
            .launch("leaf", None, "work".to_string())
            .await
            .expect("launch");
        assert!(
            at_limit.lock().get("leaf").expect("live").child.is_none(),
            "a subagent at the limit must not get a registry"
        );
    }

    /// The nested version of the Ctrl-C hang: a grandchild holds the session's
    /// tool clones too, so shutdown has to reap the whole tree, not just the
    /// first layer, or teardown cannot close the MCP children.
    #[tokio::test]
    async fn shutdown_reaps_nested_subagents_and_releases_their_tool_clones() {
        let (live, tools) = counted_tools();
        let (mut registry, _log_dir) = test_registry_at(2, 3);
        registry.ctx.mcp_tools = tools;

        let child = launch_nested(&registry).await;
        registry.shutdown().await;
        drop(child);
        drop(registry);

        assert_eq!(
            alive(&live),
            0,
            "shutdown must reap the whole tree; a grandchild left running keeps \
             a tool clone alive and teardown cannot close the MCP children"
        );
    }

    /// Releasing a subagent mid-session aborts the subagents it launched too,
    /// rather than leaving them detached and still calling tools.
    #[tokio::test(start_paused = true)]
    async fn release_aborts_the_descendant_tree() {
        let (registry, _log_dir) = test_registry_at(2, 3);
        let child = launch_nested(&registry).await;
        let grandchild_task = child.lock().get("deep").expect("live").abort.clone();

        registry.release(&["mid".to_string()]).expect("release");

        // The grandchild's task is cancelled even though it lived one level
        // below the released subagent. Abort takes effect at the next poll, so
        // yield until it lands rather than assuming a single scheduling step.
        for _ in 0..100 {
            if grandchild_task.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            grandchild_task.is_finished(),
            "releasing a subagent must abort its descendants"
        );
    }

    /// Releasing frees a name, and `release` deliberately does not wait for the
    /// cancellation it schedules. Shutdown still has to reap those tasks: a
    /// subagent released just before exit holds the session's tool clones every
    /// bit as much as a live one, and teardown's `Arc::try_unwrap` runs right
    /// after shutdown returns.
    #[tokio::test]
    async fn shutdown_reaps_released_trees() {
        let (live, tools) = counted_tools();
        let (mut registry, _log_dir) = test_registry_at(2, 3);
        registry.ctx.mcp_tools = tools;

        let child = launch_nested(&registry).await;
        registry.release(&["mid".to_string()]).expect("release");
        registry.shutdown().await;
        drop(child);
        drop(registry);

        assert_eq!(
            alive(&live),
            0,
            "a released tree must still be reaped by shutdown; dropping its \
             handles at release leaves aborted-but-unreaped tasks holding tool \
             clones, and teardown cannot close the MCP children"
        );
    }

    /// A task can be in the ledger with no name at all: [`SubagentRegistry::launch`]
    /// registers before its re-check under the lock, so the loser of a name race
    /// is spawned, aborted, and never inserted. Shutdown has to reap it anyway.
    ///
    /// The ledger state is built directly rather than by racing two launches:
    /// `launch` never suspends between its pre-check and its insert when the
    /// provider is offline, so the second call short-circuits at the pre-check
    /// and the race window cannot be forced here. What this pins down is the
    /// property the rollback path depends on -- shutdown drains the ledger, not
    /// the name map.
    #[tokio::test]
    async fn shutdown_reaps_a_task_that_has_no_name() {
        let (live, tools) = counted_tools();
        let (registry, _log_dir) = test_registry();

        // Stands in for a subagent task: it owns a clone of the session's tools
        // and never finishes on its own, so only cancellation releases them.
        let held = tools.clone();
        let task = tokio::spawn(async move {
            let _tools = held;
            std::future::pending::<()>().await;
        });
        registry.spawned_lock().push(Spawned { task, child: None });
        assert!(
            registry.lock().is_empty(),
            "this task is deliberately reachable only through the ledger"
        );

        registry.shutdown().await;
        drop(tools);
        drop(registry);

        assert_eq!(
            alive(&live),
            0,
            "a spawned task with no name still holds tool clones, so shutdown \
             must join it rather than only the ones it can find by name"
        );
    }

    /// The ledger outlives names, so it needs a sweep or a long session that
    /// cycles handles would carry a record per launch forever.
    #[tokio::test]
    async fn the_ledger_is_swept_of_reaped_tasks() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit-a", None, "work".to_string())
            .await
            .expect("launch");
        registry.release(&["audit-a".to_string()]).expect("release");

        // Abort lands at the next poll, so yield until the task is actually
        // reaped -- only then is the record eligible to be swept.
        for _ in 0..100 {
            if registry.tree_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            registry.tree_finished(),
            "the released task should be reaped"
        );

        registry
            .launch("audit-b", None, "work".to_string())
            .await
            .expect("relaunch");
        assert_eq!(
            registry.spawned_lock().len(),
            1,
            "launching should sweep records whose tasks are already reaped"
        );
    }

    #[test]
    fn set_result_instruction_is_always_present() {
        for parent in [None, Some(""), Some("   "), Some("you review Rust")] {
            let composed = compose_preamble(parent);
            assert!(
                composed.contains("outrig__set_result"),
                "missing the reporting instruction for parent {parent:?}"
            );
        }
    }

    #[test]
    fn parent_preamble_follows_the_fragment() {
        let composed = compose_preamble(Some("you review Rust"));
        let fragment = composed
            .find("outrig__set_result")
            .expect("fragment present");
        let parent = composed
            .find("you review Rust")
            .expect("parent text present");
        assert!(
            fragment < parent,
            "parent text should come after the fragment"
        );
    }

    /// The session's own preamble is deliberately not inherited: the parent
    /// decides what context is relevant, including none.
    #[test]
    fn nothing_else_is_inherited() {
        let composed = compose_preamble(None);
        assert!(!composed.contains("sandboxed container"));
    }

    #[test]
    fn accepts_short_kebab_case() {
        assert!(validate_name("audit").is_ok());
        assert!(validate_name("audit-config").is_ok());
        assert!(validate_name("check-mcp-2").is_ok());
    }

    #[test]
    fn rejects_shapes_that_are_awkward_to_repeat() {
        assert!(validate_name("").is_err());
        assert!(validate_name("Audit").is_err());
        assert!(validate_name("audit_config").is_err());
        assert!(validate_name("audit--config").is_err());
        assert!(validate_name("-audit").is_err());
        assert!(validate_name("audit-").is_err());
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
    }
}
