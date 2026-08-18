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
use outrig::config::Config;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::llm::{ResolvedAgent, TurnStop};
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
///
/// Deliberately not `Debug`: under `local-llm` this carries an
/// `Arc<LlmRegistry>` holding loaded mistralrs engines, which are not `Debug`
/// themselves, so a derive here compiles in a default build and breaks the
/// feature build.
#[derive(Clone)]
pub struct SubagentContext {
    /// The session's resolved agent. A subagent reuses its limits and sampling;
    /// the preamble is replaced by whatever the parent passes, and the model is
    /// re-resolved when the launch names one.
    pub resolved: ResolvedAgent,
    /// The merged session config, kept so a launch can re-resolve the agent
    /// against a different `[models.<name>]`. An `Arc` because the context is
    /// cloned per nesting level, and because a grandchild must re-resolve
    /// against the same config the session did.
    pub cfg: Arc<Config>,
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

    /// The merged config a launch through this registry re-resolves against.
    /// Read by [`crate::builtin_tool::parent_tools`] to build the `model`
    /// enum, so the schema and the launch agree on which names exist.
    pub(crate) fn config(&self) -> &Config {
        &self.ctx.cfg
    }

    /// The model the *launching* agent runs under -- what "omit to use yours"
    /// in the tool schema names.
    pub(crate) fn parent_model_name(&self) -> &str {
        self.ctx.resolved.model_name()
    }

    /// Launch a subagent under `name`, running `prompt` as its first round.
    /// `model` names a `[models.<name>]` to run it under; `None` inherits the
    /// launching agent's.
    ///
    /// Returns the [`ModelLabel`] the launch resolved to, `Some` on exactly the
    /// condition the transcript header is written -- the caller named a model.
    /// Handing the label back rather than letting the tool re-derive it from
    /// its own argument is what lets an alias show the hop it took, and is what
    /// makes `ModelLabel`'s claim to carry all three surfaces true.
    pub async fn launch(
        &self,
        name: &str,
        model: Option<&str>,
        preamble: Option<String>,
        prompt: String,
    ) -> Result<Option<ModelLabel>, String> {
        validate_name(name)?;
        // Resolved before the entries lock, and before anything is registered:
        // a bad model name costs one tool call and leaves the handle free for a
        // corrected retry, with no half-built entry or spawned task to unwind.
        // It is a config lookup and a struct build -- no I/O, no await -- so the
        // lock scope below stays as narrow as it was.
        let resolved = resolve_launch_model(&self.ctx, model)?;
        // `Some` iff the caller named a model, not by comparing against the
        // parent's: an inherited launch must read exactly as it did before, and
        // an agent that names its own model asked and wants confirmation.
        let label = model.map(|_| ModelLabel::of(&resolved));
        // The same value the transcript header is written from, so the trace
        // and the header cannot disagree about which model ran. Cloned because
        // `label` moves into the round loop below.
        let launched_as = label.clone();
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
        // A multi-gigabyte weight load would otherwise stall the parent's tool
        // call with no output at all, which reads as a hang. The parent's own
        // model is loaded by definition, so an inherited launch cannot get here.
        //
        // Only for a lone candidate, which is the shape whose load happens
        // *here*, inside `build_subagent_agent`. A chain defers its local rows
        // to the moment it reaches them and announces the wait there, so
        // warning at launch as well would print the same line twice for one
        // initialization -- and would be predicting a load the chain may never
        // perform, since a working primary means the fallback is never touched.
        #[cfg(feature = "local-llm")]
        if resolved.candidates.len() == 1
            && resolved.model_weights().is_some()
            && !self.ctx.registry.is_loaded(resolved.model_name())
        {
            eprintln!(
                "[outrig] subagent {name}: loading in-process model {} (first use; this may take \
                 several minutes)",
                resolved.model_name()
            );
        }
        let (agent, child) =
            build_subagent_agent(&self.ctx, &resolved, &shared, name, preamble).await?;

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
            label,
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
        Ok(launched_as)
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

/// The configured models this build could actually reach, in `cfg.models`'
/// `BTreeMap` order -- so both the tool schema's `enum` and the unknown-model
/// message are sorted and byte-stable across launches.
///
/// A name is kept only when resolution would not reject it out of hand, which
/// is [`crate::llm::selectability`]'s question -- the same predicate the
/// resolver selects alias candidates with, so the set advertised here and the
/// set a launch can actually reach cannot drift.
///
/// An **alias** is usable when *any* of its candidates is. That falls out well:
/// `alias = ["opus-local", "opus-anthropic"]` stays offerable in a build
/// without `local-llm`, where naming `opus-local` directly would not be.
///
/// Membership still does not prove a model *works* -- a key may be wrong, an
/// endpoint may be down, a GGUF path may not exist -- only that it is not a
/// guaranteed failure, which is the right bar for something advertised to the
/// model. An *unset* api-key variable is on the guaranteed side of that line,
/// since the resolver fails the session on it eagerly, so it excludes a name
/// here too.
///
/// Pure inspection of `Config` plus the environment: no registry touch, no
/// weight load, no client construction, so the synchronous schema-building
/// path can call it.
pub(crate) fn usable_model_names(cfg: &Config) -> Vec<String> {
    cfg.models
        .keys()
        .filter(|name| {
            // A walk error means the alias graph is malformed, which is not
            // something to advertise. `usable_model_names` is infallible by
            // contract, so an uncheckable name is simply not offered.
            cfg.model_candidates(name)
                .is_ok_and(|candidates| crate::llm::first_selectable(cfg, &candidates).is_ok())
        })
        .cloned()
        .collect()
}

/// The model a subagent was launched under, for the three places a subagent is
/// already visible: the launch trace, the tool result the parent reads, and the
/// transcript header. One value carries all three so they cannot drift.
///
/// `None` wherever the launch inherited the parent's model, which is what keeps
/// the default path byte-for-byte -- and byte-free in the transcript -- as it
/// was before this argument existed.
///
/// Display-only. `name` holds the rendered `alias -> concrete` hop when one was
/// taken, so it is not a key anything can be looked up by.
#[derive(Clone, Debug)]
pub(crate) struct ModelLabel {
    name: String,
    provider: String,
    identifier: String,
}

impl ModelLabel {
    /// Built once at launch, from the *first* candidate.
    ///
    /// That is the model the subagent starts on, and for a chain it is a
    /// preference rather than a certainty: a mid-turn move changes which model
    /// is answering without changing this label. Attribution that has to be
    /// exact about what served a given reply cannot read it -- see
    /// `plan/next/chain-attribution-names-the-first-candidate.md`.
    fn of(resolved: &ResolvedAgent) -> Self {
        Self {
            // `model_display`, not `model_name`: a launch under an alias shows
            // the hop it took. Identical to `model_name` for a direct name, so
            // the pre-alias output is unchanged byte for byte.
            name: resolved.model_display(),
            provider: resolved.provider_name().to_string(),
            identifier: resolved.model_identifier().to_string(),
        }
    }

    /// The model as the launch trace and the tool result name it -- the first
    /// slot of [`detail`](Self::detail), without the provider and identifier
    /// those two surfaces have never carried.
    pub(crate) fn display_name(&self) -> &str {
        &self.name
    }

    /// The transcript header's parenthesized detail: the name the agent asked
    /// for, plus the provider and wire identifier it started on. See
    /// [`of`](Self::of) for why "started" rather than "landed on".
    fn detail(&self) -> String {
        format!(
            "model: {} / provider: {} / {}",
            self.name, self.provider, self.identifier
        )
    }
}

/// The tool-boundary message for a name no configured model can serve.
///
/// Composed here rather than by widening `LlmResolveError::UnknownModel`: the
/// enumeration is build-specific, which is a tool-boundary fact and not a
/// resolver one, and leaving the resolver alone keeps the `--model` CLI message
/// untouched. The list comes from [`usable_model_names`], so this text and the
/// schema's `enum` can never disagree.
fn unusable_model_message(cfg: &Config, model: &str) -> String {
    format!(
        "no usable model named {model:?}; available: {}",
        usable_model_names(cfg).join(", ")
    )
}

/// The whole model decision for one launch.
///
/// `None` clones the context's `ResolvedAgent` and does nothing else -- the same
/// clone a launch has always made. `Some(model)` re-resolves the *parent's*
/// agent against that model, then overwrites the carried-forward fields from the
/// parent.
///
/// The overwrite direction is deliberate: assigning the parent's values onto the
/// fresh resolution means a field added to `ResolvedAgent` later arrives on the
/// carried-forward side, which is the safe default. It is also what preserves
/// `run_inner`'s post-resolution `--max-tool-calls` / `--max-tool-result-bytes`
/// overrides, which a fresh resolution would replace with config defaults.
///
/// `max_tokens` is the one exception, and is left on the re-resolved side. An
/// output-token ceiling belongs to the model rather than to the agent: a cheap
/// model serves fewer output tokens than an expensive one, so carrying the
/// parent's ceiling forward sends a number the named model rejects -- and it does
/// so in exactly the case the `model` argument exists for, an expensive parent
/// delegating down. The re-resolution already computed
/// `agent.max-tokens.or(named_model.max-tokens)`, which keeps a deliberate
/// per-agent ceiling winning over the model's default.
fn resolve_launch_model(
    ctx: &SubagentContext,
    model: Option<&str>,
) -> Result<ResolvedAgent, String> {
    let Some(model) = model else {
        return Ok(ctx.resolved.clone());
    };
    // `None` for the device override: a subagent names a model, not hardware.
    match crate::llm::resolve_agent_with_overrides(
        &ctx.cfg,
        ctx.resolved.agent_name.as_deref(),
        Some(model),
        None,
    ) {
        Ok(mut resolved) => {
            let parent = &ctx.resolved;
            resolved.preamble = parent.preamble.clone();
            resolved.temperature = parent.temperature;
            resolved.tool_call_max = parent.tool_call_max;
            resolved.tool_result_max_bytes = parent.tool_result_max_bytes;
            resolved.subagent_depth_max = parent.subagent_depth_max;
            resolved.subagent_width_max = parent.subagent_width_max;
            resolved.image = parent.image.clone();
            Ok(resolved)
        }
        // Discriminated by matched variant, not by membership in the usable
        // set: membership would collapse a local model in a default build into
        // "unknown", and that case has to keep surfacing
        // `MistralrsFeatureDisabled` so the remedy (a rebuild) is legible. That
        // variant is merely unconstructible without the feature rather than
        // `cfg`-gated, so this match compiles identically in both builds.
        Err(crate::error::CliError::LlmResolve(
            crate::llm::LlmResolveError::UnknownModel { .. }
            | crate::llm::LlmResolveError::UnknownProvider { .. }
            | crate::llm::LlmResolveError::UnsupportedProvider { .. },
        )) => Err(unusable_model_message(&ctx.cfg, model)),
        Err(e) => Err(e.to_string()),
    }
}

/// Build the agent loop one subagent runs, and, when depth allows, the registry
/// it launches its own subagents through.
///
/// The `resolved` handed in is whatever [`resolve_launch_model`] decided, so a
/// launch that named a model arrives here already re-resolved; only the
/// preamble is replaced. The tool list is always the session's MCP tools plus
/// this subagent's own `outrig__set_result`. When the subagent's depth is under
/// `subagent_depth_max`, it also gets its own [`SubagentRegistry`] and the
/// parent-side launch tools, so it can launch children of its own; at the max
/// depth those are withheld and the returned registry is `None`. Recursion is
/// bounded by that depth check rather than impossible by construction.
async fn build_subagent_agent(
    ctx: &SubagentContext,
    resolved: &ResolvedAgent,
    shared: &Arc<SubagentShared>,
    name: &str,
    preamble: Option<String>,
) -> Result<(crate::llm::RigAgent, Option<Arc<SubagentRegistry>>), String> {
    let mut resolved = resolved.clone();
    // A subagent always carries the `set_result` protocol fragment, so its
    // preamble is `Some` even when the session that launched it has none.
    resolved.preamble = Some(compose_preamble(preamble.as_deref()));

    let mut tools = ctx.mcp_tools.clone();
    // The agent name is the parent's -- a subagent has no `[agents.<name>]` table
    // of its own -- but the ceiling has to be *this* subagent's, which a launch
    // that named a model resolved from that model. Reporting the parent's would
    // name a number that was never in effect for the report being truncated.
    tools.push(SessionTool::new(crate::builtin_tool::SetResultTool::new(
        shared.clone(),
        name,
        ctx.resolved.agent_name.as_deref(),
        resolved.max_tokens(),
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
    label: Option<ModelLabel>,
) {
    let mut history = Vec::new();
    let mut log = transcript::Transcript::open(&log_dir, &name, label.as_ref()).await;

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
                match end.stopped {
                    // A broken endpoint is a failed round, not a model that
                    // declined to report, and a parent needs to tell them apart
                    // to decide whether retrying is worth anything. Publishing
                    // also keeps reads edge-triggered: `note_ended_early`
                    // records a cause without bumping the version, so a parent
                    // polling a subagent whose endpoint is down would otherwise
                    // be handed the same answer forever instead of blocking for
                    // something new.
                    Some(TurnStop::EndpointFailed(reason)) => {
                        publish_round_failure(&shared, &name, &reason);
                    }
                    // Recorded before `end_round` so that a round which
                    // published nothing can tell the parent *why* it stopped
                    // instead of only that it did. Does not displace a
                    // truncated report: running out of tool calls is what
                    // follows from a report that would not fit.
                    Some(TurnStop::Interrupted(reason)) => shared.note_ended_early(reason),
                    // A round that finished on its own and produced no text at
                    // all. Without this the parent is told only that the
                    // subagent "stopped without calling outrig__set_result",
                    // which blames the model for declining to report when in
                    // fact it never got a turn out at all -- and the two want
                    // different responses from the parent.
                    None if end.is_silent() => {
                        shared.note_ended_early(end.silent_reason().to_string());
                    }
                    None => {}
                }
            }
            Err(e) => publish_round_failure(&shared, &name, &e.to_string()),
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

/// Publish a round failure to the parent. Publish only -- the transcript entry
/// comes from the outcome block at the end of the round, which would otherwise
/// record it twice.
fn publish_round_failure(shared: &SubagentShared, name: &str, reason: &str) {
    let message = format!("round failed: {reason}");
    eprintln!("[outrig]   [{name}] {message}");
    shared.publish(Outcome::Error(message));
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

/// Fixtures shared by this module's tests and [`crate::builtin_tool`]'s schema
/// tests, which have to build a registry exactly the way a launch test does or
/// the schema they assert on is not the one a launch would produce.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    use crate::llm::{ResolvedCandidate, ResolvedProvider};
    use outrig::config::{Agent, ApiKeyRef, LlmProvider, Model, OpenAiOptions};

    /// The env var the fixture provider's api-key points at. Unique to this
    /// module so concurrent tests cannot race another fixture on the same key.
    const KEY_VAR: &str = "OUTRIG_TEST_SUBAGENT_MODEL_KEY";

    /// Provider and agent tables shared by both configs below: one hosted
    /// provider at the discard port, so a round fails immediately, and the one
    /// agent the contexts name.
    fn base_config() -> Config {
        // SAFETY: edition 2024 marks `env::set_var` unsafe because of
        // multi-thread races. The name is unique to this fixture and the value
        // never varies, so a concurrent write is writing the same bytes.
        unsafe { std::env::set_var(KEY_VAR, "test-key") };

        let mut cfg = Config::default();
        cfg.providers.insert(
            "openai".to_string(),
            LlmProvider::openai(
                "http://127.0.0.1:9",
                ApiKeyRef::parse(&format!("${{{KEY_VAR}}}")).expect("api-key ref parses"),
                OpenAiOptions::new().with_request_timeout_secs(1),
            ),
        );
        let mut agent = Agent::default();
        agent.model = Some("smart".to_string());
        agent.preamble = Some("session preamble".to_string());
        cfg.agents.insert("primary".to_string(), agent);
        cfg
    }

    fn hosted_model(identifier: &str, max_tokens: Option<u32>) -> Model {
        let mut model = Model::new("openai");
        model.identifier = Some(identifier.to_string());
        model.max_tokens = max_tokens;
        model
    }

    /// Two usable models, so the schema advertises a choice and a launch has
    /// something other than the parent's model to name.
    ///
    /// The two carry different ceilings on purpose: a cheap model serves fewer
    /// output tokens than an expensive one, and that asymmetry runs the same
    /// direction as the delegation the `model` argument exists for.
    pub(crate) fn test_config() -> Config {
        let mut cfg = base_config();
        cfg.models.insert(
            "fast".to_string(),
            hosted_model("gpt-4o-mini", Some(16_000)),
        );
        cfg.models
            .insert("smart".to_string(), hosted_model("gpt-4o", Some(64_000)));
        cfg
    }

    /// Only the parent's own model, which is the case where the `model`
    /// property is left out of the schema entirely.
    pub(crate) fn test_config_single() -> Config {
        let mut cfg = base_config();
        cfg.models
            .insert("smart".to_string(), hosted_model("gpt-4o", None));
        cfg
    }

    /// [`test_config`] plus `cheap`, an alias onto `fast`.
    ///
    /// A separate fixture rather than a third model on `test_config`, because
    /// the schema-enum test pins that config's `enum` positionally -- adding a
    /// name there would break the byte-stability assertion it exists to make.
    pub(crate) fn alias_config() -> Config {
        let mut cfg = test_config();
        cfg.models
            .insert("cheap".to_string(), Model::alias(["fast"]));
        cfg
    }

    /// An alias spanning an in-process candidate and a hosted one, in that
    /// order. Usable in *both* builds: with `local-llm` the first candidate
    /// wins, and without it the alias falls through to the hosted one -- which
    /// is the case naming `onprem` directly cannot express.
    pub(crate) fn mixed_alias_config() -> Config {
        let mut cfg = local_model_config();
        cfg.models
            .insert("either".to_string(), Model::alias(["onprem", "fast"]));
        cfg
    }

    /// [`test_config`] plus an in-process model, `onprem`. Usable only in a
    /// `local-llm` build, which is exactly what makes it useful in both: one
    /// build resolves it, the other must refuse it with the feature-disabled
    /// error rather than "unknown model".
    pub(crate) fn local_model_config() -> Config {
        let mut cfg = test_config();
        cfg.providers
            .insert("local".to_string(), LlmProvider::Mistralrs);
        let mut model = Model::new("local");
        model.model_id = Some("Qwen/Qwen2.5-7B-Instruct".to_string());
        cfg.models.insert("onprem".to_string(), model);
        cfg
    }

    /// The session's resolved agent as the context carries it: `smart`, the
    /// model the fixture agent is configured on, against the discard port.
    pub(crate) fn test_resolved(subagent_depth_max: u32) -> ResolvedAgent {
        // Discard port: connects are refused immediately.
        test_resolved_at("http://127.0.0.1:9", None, subagent_depth_max)
    }

    /// [`test_resolved`] with retries switched off, for the real-clock tests
    /// that need a round against the discard port to fail on the first refused
    /// connect rather than ride the connect budget out.
    ///
    /// The default is deliberately *not* this: a fixture that pins retries off
    /// to stay fast is compensating for the retry classification, which is the
    /// thing the connect budget fixed. Measuring something other than retry is
    /// the case that earns the opt-out, and it says so at the call site.
    pub(crate) fn test_resolved_without_retries(subagent_depth_max: u32) -> ResolvedAgent {
        test_resolved_at("http://127.0.0.1:9", Some(0), subagent_depth_max)
    }

    /// [`test_resolved`] against a caller-supplied endpoint, for the tests that
    /// need the round to reach a model rather than fail connecting.
    ///
    /// `retry_budget_secs` is a parameter rather than something a caller pokes
    /// afterwards, so "retries off" is a value this one constructor understands
    /// and cannot be lost by a fixture changing provider variant.
    pub(crate) fn test_resolved_at(
        base_url: &str,
        retry_budget_secs: Option<u64>,
        subagent_depth_max: u32,
    ) -> ResolvedAgent {
        ResolvedAgent {
            agent_name: Some("primary".to_string()),
            candidates: vec![ResolvedCandidate {
                model_name: "smart".to_string(),
                model_identifier: "gpt-4o".to_string(),
                provider_name: "openai".to_string(),
                provider: ResolvedProvider::OpenAi {
                    base_url: base_url.to_string(),
                    api_key: "test-key".to_string(),
                    request_timeout_secs: Some(1),
                    // `None` at every call site but one, which keeps
                    // "immediately" true on its own: a refused connection never
                    // reaches the endpoint, so the short connect budget bounds
                    // it rather than the full one.
                    retry_budget_secs,
                },
                model_weights: None,
                max_tokens: None,
            }],
            alias_name: None,
            preamble: Some("session preamble".to_string()),
            temperature: None,
            tool_call_max: 4,
            tool_result_max_bytes: 4096,
            subagent_depth_max,
            subagent_width_max: outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX,
            image: None,
        }
    }

    /// A registry over `cfg`, launching at `depth` under `subagent_depth_max`.
    pub(crate) fn registry_with(
        cfg: Config,
        depth: u32,
        subagent_depth_max: u32,
    ) -> (SubagentRegistry, tempfile::TempDir) {
        let log_dir = tempfile::tempdir().expect("tempdir");
        let registry = SubagentRegistry::new(SubagentContext {
            resolved: test_resolved(subagent_depth_max),
            cfg: Arc::new(cfg),
            mcp_tools: Vec::new(),
            cache_root: PathBuf::from("."),
            log_dir: log_dir.path().to_path_buf(),
            depth,
            #[cfg(feature = "local-llm")]
            registry: Arc::new(crate::llm::LlmRegistry::new()),
        });
        (registry, log_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Notify;

    /// A registry whose subagents talk to a closed port, so every round fails
    /// fast and deterministically. That is enough to exercise the bookkeeping
    /// -- launch, the inbox, watermarks, release -- without a live model.
    ///
    /// "Fast" no longer needs a fixture-side `retry_budget_secs: Some(0)`: a
    /// refused connection never reaches the endpoint, so the retry loop bounds
    /// it by the short connect budget instead of the full one. The tests below
    /// still run with `start_paused`, which keeps them immune to any wait the
    /// rest of the round picks up.
    fn test_registry() -> (SubagentRegistry, tempfile::TempDir) {
        test_registry_at(2, outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX)
    }

    /// Like [`test_registry`] but with an explicit launch depth and depth limit,
    /// so a test can place its subagents just under or right at the ceiling.
    fn test_registry_at(
        depth: u32,
        subagent_depth_max: u32,
    ) -> (SubagentRegistry, tempfile::TempDir) {
        registry_with(test_config(), depth, subagent_depth_max)
    }

    /// A stand-in for the session's MCP tools that counts how many copies are
    /// alive, so a test can *observe* release rather than infer it. Every live
    /// subagent task owns a clone of the session's tool list, which is what
    /// teardown's `Arc::try_unwrap` needs released before it can shut the MCP
    /// children down.
    ///
    /// Returns the counter seeded at 1 for the copy the caller installs on the
    /// registry; a test reaches 0 only after dropping the registry too.
    fn counted_tools() -> (Arc<AtomicUsize>, Vec<SessionTool>) {
        let live = Arc::new(AtomicUsize::new(1));
        let tools = vec![SessionTool::new(CountedTool {
            name: "counted",
            live: live.clone(),
            blocking: None,
        })];
        (live, tools)
    }

    /// How many copies of a [`CountedTool`] are still alive. Zero means every
    /// clone has been dropped.
    fn alive(counter: &AtomicUsize) -> usize {
        counter.load(Ordering::SeqCst)
    }

    /// The session-tool stand-in behind both [`counted_tools`] and
    /// [`BlockingTools`]. `blocking` is what separates them: `None` returns
    /// immediately, `Some` announces that the call was entered and then never
    /// returns, so the round is still inside the tool when shutdown starts.
    struct CountedTool {
        name: &'static str,
        live: Arc<AtomicUsize>,
        blocking: Option<Entered>,
    }

    /// The readiness half of a blocking [`CountedTool`].
    struct Entered {
        count: Arc<AtomicUsize>,
        ready: Arc<Notify>,
    }

    impl Drop for CountedTool {
        fn drop(&mut self) {
            self.live.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl rig::tool::ToolDyn for CountedTool {
        fn name(&self) -> String {
            self.name.to_string()
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
        ) -> rig::wasm_compat::WasmBoxedFuture<'a, Result<String, rig::tool::ToolError>> {
            Box::pin(async move {
                let Some(entered) = &self.blocking else {
                    return Ok(String::new());
                };
                entered.count.fetch_add(1, Ordering::SeqCst);
                // There is one readiness waiter. notify_one retains a permit if
                // the increment lands between its predicate check and await,
                // unlike notify_waiters.
                entered.ready.notify_one();
                std::future::pending().await
            })
        }
    }

    /// A session tool that proves every round reached a tool call and then
    /// remains in flight until shutdown cancels its owning subagent task.
    struct BlockingTools {
        /// Live clones, seeded at 1 for the copy installed on the registry --
        /// same convention as [`counted_tools`].
        live: Arc<AtomicUsize>,
        entered: Arc<AtomicUsize>,
        ready: Arc<Notify>,
        tools: Vec<SessionTool>,
    }

    impl BlockingTools {
        fn new() -> Self {
            let live = Arc::new(AtomicUsize::new(1));
            let entered = Arc::new(AtomicUsize::new(0));
            let ready = Arc::new(Notify::new());
            let tools = vec![SessionTool::new(CountedTool {
                // `mock_tool_call` names this tool in the call it returns.
                name: "blocking",
                live: live.clone(),
                blocking: Some(Entered {
                    count: entered.clone(),
                    ready: ready.clone(),
                }),
            })];
            Self {
                live,
                entered,
                ready,
                tools,
            }
        }

        /// Hand the tools to the registry under test.
        ///
        /// They are moved out rather than cloned: the probe outlives the
        /// registry so a test can read `live` after teardown, and a clone
        /// parked here would be a clone teardown could never release.
        fn take_tools(&mut self) -> Vec<SessionTool> {
            std::mem::take(&mut self.tools)
        }

        /// Block until `expected` rounds are suspended inside the tool.
        async fn wait_for_calls(&self, expected: usize) {
            tokio::time::timeout(SETUP_TIMEOUT, async {
                while self.entered.load(Ordering::SeqCst) < expected {
                    self.ready.notified().await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "only {} of {expected} subagents entered their tool call",
                    self.entered.load(Ordering::SeqCst)
                )
            });
        }
    }

    /// How long a full-tree test waits for setup to settle before it gives up.
    /// Generous on purpose: it is a stuck-test backstop, not a measurement --
    /// the number under test is the shutdown join, timed separately.
    const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

    /// One depth-2 subagent, the private registry it was given, and the
    /// depth-3 leaves launched through that registry.
    struct Branch {
        root: String,
        registry: Arc<SubagentRegistry>,
        leaves: Vec<String>,
    }

    /// A full tree at the shipped defaults: the session registry's eight
    /// children, and eight grandchildren in each child's private registry.
    /// Names are globally unique because transcripts share one directory.
    struct FullTree {
        branches: Vec<Branch>,
    }

    impl FullTree {
        fn live_count(&self) -> usize {
            self.branches.iter().map(|b| 1 + b.leaves.len()).sum()
        }

        /// Every subagent in the tree, as the registry that owns it and the
        /// name it is filed under.
        fn members<'a>(
            &'a self,
            root: &'a SubagentRegistry,
        ) -> impl Iterator<Item = (&'a SubagentRegistry, &'a String)> {
            self.branches.iter().flat_map(move |b| {
                std::iter::once((root, &b.root))
                    .chain(b.leaves.iter().map(|leaf| (&*b.registry, leaf)))
            })
        }

        /// Block until every round has published and gone back to its prompt
        /// loop, so setup cannot leak into the shutdown measurement.
        ///
        /// `end_round` transitions through the same `watch` channel the parent
        /// reads, so waiting on that channel needs no polling -- and `wait_for`
        /// checks the current value before parking, which is what makes the
        /// `publish`-then-`end_round` ordering a non-issue. A fresh subagent
        /// starts `Running` (see `SubagentShared::new`), so no receiver can
        /// match before its round has run.
        async fn wait_until_idle(&self, root: &SubagentRegistry) {
            let mut receivers: Vec<_> = self
                .members(root)
                .map(|(registry, name)| {
                    registry
                        .lock()
                        .get(name)
                        .expect("every launched subagent stays live")
                        .shared
                        .subscribe()
                })
                .collect();

            let settled =
                futures_util::future::join_all(receivers.iter_mut().map(|rx| {
                    rx.wait_for(|snap| matches!(snap.state, state::RunState::Idle { .. }))
                }));
            tokio::time::timeout(SETUP_TIMEOUT, settled)
                .await
                .expect("every subagent finishes its round")
                .into_iter()
                .for_each(|seen| {
                    seen.expect("a subagent's state channel outlives its round");
                });
        }
    }

    /// The registry a live subagent was handed to launch its own children
    /// through. Panics if `name` is not live, or is a leaf at the depth limit.
    fn child_registry(registry: &SubagentRegistry, name: &str) -> Arc<SubagentRegistry> {
        registry
            .lock()
            .get(name)
            .expect("live")
            .child
            .clone()
            .expect("a subagent below the depth limit gets a registry")
    }

    /// Launch the widest, deepest tree the shipped defaults admit, through the
    /// real `launch` path rather than by writing the ledger directly.
    ///
    /// `registry` must come from [`test_registry`], which launches at depth 2
    /// under the shipped depth limit; the width comes from the shipped constant.
    ///
    /// Both shipped defaults are pinned against their constants rather than
    /// against the literals the caller passed, so that moving either one fails
    /// here instead of quietly measuring a tree that is no longer the worst
    /// case. At 8 and 3 the tree is 8 + 8 * 8 = 72.
    async fn launch_full_default_tree(registry: &SubagentRegistry) -> FullTree {
        let width = outrig::config::DEFAULT_SUBAGENT_WIDTH_MAX as usize;
        assert_eq!(width, 8, "this measurement pins the shipped default width");
        assert_eq!(
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
            3,
            "this measurement pins the shipped default depth: at 3 the depth-2 \
             children get registries and the depth-3 layer is leaves"
        );

        let mut branches = Vec::with_capacity(width);
        for parent_index in 0..width {
            let root = format!("root-{parent_index}");
            registry
                .launch(&root, None, None, "work".to_string())
                .await
                .expect("root launch");
            let child = child_registry(registry, &root);

            let mut leaves = Vec::with_capacity(width);
            for leaf_index in 0..width {
                let leaf = format!("leaf-{parent_index}-{leaf_index}");
                child
                    .launch(&leaf, None, None, "work".to_string())
                    .await
                    .expect("leaf launch");
                leaves.push(leaf);
            }
            branches.push(Branch {
                root,
                registry: child,
                leaves,
            });
        }

        FullTree { branches }
    }

    /// A completion that always asks for the `blocking` tool, so every round
    /// that reaches the model ends up suspended inside a session tool.
    ///
    /// The header is set by hand rather than through `axum::Json`, which would
    /// mean turning on axum's `json` feature for one test.
    async fn mock_tool_call() -> ([(&'static str, &'static str); 1], String) {
        let body = serde_json::json!({
            "id": "chatcmpl-shutdown",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_shutdown",
                        "type": "function",
                        "function": {"name": "blocking", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        });
        ([("content-type", "application/json")], body.to_string())
    }

    struct ToolCallServer {
        base_url: String,
        task: JoinHandle<()>,
    }

    impl Drop for ToolCallServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn start_tool_call_server() -> ToolCallServer {
        use axum::routing::post;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let address = listener.local_addr().expect("mock provider address");
        let app = axum::Router::new().route("/v1/chat/completions", post(mock_tool_call));
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock provider serves");
        });
        ToolCallServer {
            base_url: format!("http://{address}/v1"),
            task,
        }
    }

    /// Tear `tree` down and report what the join cost against the grace.
    ///
    /// The subject is `shutdown_tree`, the *unclamped* join, wrapped here in
    /// the same budget `shutdown` gives it. Timing `shutdown` instead would
    /// make the assertion nearly self-fulfilling: it swallows its own timeout
    /// and returns at roughly the grace no matter how wedged the tree is, so a
    /// clock read afterwards can only fail by timer overshoot. This way the
    /// bound is the timeout's own verdict, and the elapsed time is reported for
    /// the margin rather than asked to stand in for the bound.
    ///
    /// Takes both by value so drop order is settled here rather than repeated
    /// per test: `tree` holds the child registries, and each one's context
    /// carries a clone of the session's tools that teardown needs released.
    async fn shutdown_within_grace(
        shape: &str,
        registry: SubagentRegistry,
        tree: FullTree,
        live: &AtomicUsize,
    ) {
        let live_count = tree.live_count();
        let started = Instant::now();
        let joined = tokio::time::timeout(SHUTDOWN_GRACE, registry.shutdown_tree()).await;
        let elapsed = started.elapsed();

        // Visible under `--nocapture`; the margin is the point, not the pass.
        eprintln!(
            "[outrig] {shape} {live_count}-subagent tree joined in {elapsed:?} \
             (grace {SHUTDOWN_GRACE:?})"
        );
        assert!(
            joined.is_ok(),
            "{shape} full-tree shutdown did not join within {SHUTDOWN_GRACE:?}"
        );

        drop(tree);
        drop(registry);
        assert_eq!(
            alive(live),
            0,
            "{shape} full-tree shutdown must release every session tool clone"
        );
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
            .launch("mid", None, None, "work".to_string())
            .await
            .expect("launch");
        let child = child_registry(registry, "mid");
        child
            .launch("deep", None, None, "work".to_string())
            .await
            .expect("grandchild launch");
        child
    }

    /// Exercises the whole path: launch spawns a round, the round fails, the
    /// driver publishes the failure, and the parent collects it.
    ///
    /// The fixture's provider points at the discard port, so this travels the
    /// `endpoint_failed` arm -- a connection refused is transient, so it ends
    /// the turn cleanly rather than erroring out of `run_turn_captured`. That
    /// it still reaches the parent as an `Error`, and that the next read blocks
    /// (see the test below), is the whole point of that arm: `note_ended_early`
    /// would record the cause without bumping the version, leaving the parent
    /// re-reading the same answer forever.
    #[tokio::test(start_paused = true)]
    async fn a_failed_round_reaches_the_parent_as_an_error() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, None, "check the config".to_string())
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
            .launch("audit", None, None, "check the config".to_string())
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
                .launch(name, None, None, "work".to_string())
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
            .launch("audit", None, None, "work".to_string())
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
            .launch("audit", None, None, "work".to_string())
            .await
            .expect("first launch");
        registry.get_result("audit").await.expect("collect result");

        let err = registry
            .launch("second", None, None, "more work".to_string())
            .await
            .expect_err("an idle but unreleased handle still consumes the slot");
        assert!(err.contains("width limit of 1"), "got: {err}");
        assert!(err.contains("outrig__subagent_release"), "got: {err}");

        registry.release(&["audit".to_string()]).expect("release");
        registry
            .launch("second", None, None, "more work".to_string())
            .await
            .expect("release must free a width slot");
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_name_cannot_be_reused() {
        let (registry, _log_dir) = test_registry();
        registry
            .launch("audit", None, None, "work".to_string())
            .await
            .expect("first launch");

        let err = registry
            .launch("audit", None, None, "other work".to_string())
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
            .launch("audit", None, None, "work".to_string())
            .await
            .expect("launch");
        registry.get_result("audit").await.expect("collect");
        registry
            .release(&["audit".to_string()])
            .expect("release succeeds");

        registry
            .launch("audit", None, None, "work again".to_string())
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
            .launch("audit-a", None, None, "work".to_string())
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
            .launch("audit", None, None, "work".to_string())
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
            .launch("audit", None, None, "work".to_string())
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
                .launch(name, None, None, "work".to_string())
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
            .launch("audit", None, None, "work".to_string())
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
            .launch("mid", None, None, "work".to_string())
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
            .launch("deep", None, None, "work".to_string())
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
            .launch("mid", None, None, "work".to_string())
            .await
            .expect("launch");
        assert!(
            below.lock().get("mid").expect("live").child.is_some(),
            "a subagent below the limit should get a registry to launch with"
        );

        // depth 3 with max 3: 3 == 3, so the subagent is a leaf.
        let (at_limit, _log_b) = test_registry_at(3, 3);
        at_limit
            .launch("leaf", None, None, "work".to_string())
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

    /// Measure the shipped worst case after every round has published and
    /// returned to its prompt loop. This is the largest idle handle set the
    /// default width and depth permit, not a synthetic ledger-only tree.
    ///
    /// Run with `--nocapture` to see the join time it measures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_idle_tree_shuts_down_within_the_grace() {
        let (live, tools) = counted_tools();
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.mcp_tools = tools;
        // What this measures is the join, on a real clock, after every round
        // has gone idle -- so the rounds themselves have to fail at once. The
        // fixture's default budget is realistic on purpose, which here would
        // mean spending the whole connect budget on backoff before the tree
        // could settle, timing the shutdown grace behind 30s of retry.
        registry.ctx.resolved =
            test_resolved_without_retries(outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX);

        let tree = launch_full_default_tree(&registry).await;
        tree.wait_until_idle(&registry).await;

        shutdown_within_grace("idle", registry, tree, &live).await;
    }

    /// Measure the same real tree in the harder shape: every round has entered
    /// a session tool and is suspended inside that call when shutdown begins.
    /// That is the case the grace exists to bound -- an idle tree has nothing
    /// to interrupt.
    ///
    /// Needs a provider that answers, so this one points at a loopback endpoint
    /// that always calls the blocking tool, rather than the fixture's discard
    /// port where the round would fail before reaching a tool at all.
    ///
    /// Run with `--nocapture` to see the join time it measures.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn full_in_flight_tool_tree_shuts_down_within_the_grace() {
        let server = start_tool_call_server().await;
        let mut probe = BlockingTools::new();
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.mcp_tools = probe.take_tools();
        registry.ctx.resolved = test_resolved_at(
            &server.base_url,
            None,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );

        let tree = launch_full_default_tree(&registry).await;
        probe.wait_for_calls(tree.live_count()).await;

        shutdown_within_grace("in-flight-tool", registry, tree, &probe.live).await;
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
            .launch("audit-a", None, None, "work".to_string())
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
            .launch("audit-b", None, None, "work".to_string())
            .await
            .expect("relaunch");
        assert_eq!(
            registry.spawned_lock().len(),
            1,
            "launching should sweep records whose tasks are already reaped"
        );
    }

    /// The default path: no model named means the context's resolution is used
    /// as-is, with no second resolve to introduce a difference.
    #[tokio::test(start_paused = true)]
    async fn launch_without_model_inherits_parent_resolution() {
        let (registry, _log_dir) = test_registry();
        let resolved = resolve_launch_model(&registry.ctx, None).expect("inherits");
        assert_eq!(resolved, registry.ctx.resolved);
    }

    /// The left column of the field-split table: the model-derived fields come
    /// from the named model, and the agent identity does not.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_reresolves_model_fields() {
        let (registry, _log_dir) = test_registry();
        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");

        assert_eq!(resolved.model_name(), "fast");
        assert_eq!(resolved.model_identifier(), "gpt-4o-mini");
        assert_eq!(resolved.provider_name(), "openai");
        assert_eq!(
            resolved.agent_name, registry.ctx.resolved.agent_name,
            "the agent is still the parent's -- SetResultTool's trace prefix \
             depends on it"
        );
        assert_eq!(resolved.preamble, registry.ctx.resolved.preamble);
        assert_eq!(
            registry.ctx.resolved.model_name(),
            "smart",
            "the session's own resolution must be untouched -- the parent keeps \
             taking its turns on its own model"
        );
    }

    /// The named regression: `--max-tool-calls` / `--max-tool-result-bytes` are
    /// applied to the session's `ResolvedAgent` *after* resolution, so a launch
    /// that took its limits from a fresh resolve would silently drop them.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_preserves_cli_overrides() {
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.resolved.tool_call_max = 7;
        registry.ctx.resolved.tool_result_max_bytes = 1234;

        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");
        assert_eq!(
            (resolved.tool_call_max, resolved.tool_result_max_bytes),
            (7, 1234),
            "the session's CLI overrides must survive a re-resolution; a fresh \
             resolve would hand back config defaults here"
        );
    }

    /// Sampling is an agent knob, so it still crosses a re-resolution untouched.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_inherits_sampling() {
        let (mut registry, _log_dir) = test_registry();
        registry.ctx.resolved.temperature = Some(0.25);

        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");
        assert_eq!(resolved.temperature, Some(0.25));
    }

    /// The ceiling is a model knob, and is the one field that does *not* carry
    /// forward: the parent's belongs to the parent's model, and sending it at a
    /// cheaper one is a request that model refuses outright.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_takes_the_ceiling_from_that_model() {
        let (mut registry, _log_dir) = test_registry();
        // What the session resolved for itself, from `[models.smart]`.
        registry.ctx.resolved.candidates[0].max_tokens = Some(64_000);

        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");
        assert_eq!(
            resolved.max_tokens(),
            Some(16_000),
            "the ceiling must follow the model named, not the launching agent"
        );
    }

    /// `agent.or(model)` has to survive the re-resolution: an operator who set a
    /// ceiling on the agent meant it for every model that agent runs.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_keeps_the_agent_ceiling_over_the_models() {
        let mut cfg = test_config();
        cfg.agents
            .get_mut("primary")
            .expect("fixture agent")
            .max_tokens = Some(8_000);
        // The parent's own ceiling is left unset, so 8000 can only have come
        // from the re-resolution. Pinning it to 8000 too would let a restored
        // carry-forward pass this test.
        let (registry, _log_dir) = registry_with(cfg, 0, 2);

        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");
        assert_eq!(
            resolved.max_tokens(),
            Some(8_000),
            "an agent that set a ceiling meant it for every model it runs, so \
             it still beats the named model's own"
        );
    }

    /// Depth and image are the parent's too, and naming a model does not buy a
    /// launch past the depth ceiling.
    #[tokio::test(start_paused = true)]
    async fn launch_with_model_inherits_depth_and_image() {
        let (mut registry, _log_dir) = registry_with(test_config(), 3, 3);
        registry.ctx.resolved.image = Some("parent-image".to_string());

        let resolved = resolve_launch_model(&registry.ctx, Some("fast")).expect("resolves");
        assert_eq!(resolved.subagent_depth_max, 3);
        assert_eq!(resolved.image.as_deref(), Some("parent-image"));

        // depth 3 with max 3: at the ceiling, so the subagent is still a leaf.
        registry
            .launch("leaf", Some("fast"), None, "work".to_string())
            .await
            .expect("launch");
        assert!(
            registry.lock().get("leaf").expect("live").child.is_none(),
            "naming a model must not exempt a launch from the depth limit"
        );
    }

    /// Refusal, enumeration, no half-registration, and a corrected retry -- the
    /// four things the acceptance bullet asks of a bad name.
    #[tokio::test(start_paused = true)]
    async fn unknown_model_refuses_launch_and_leaves_handle_free() {
        let (registry, _log_dir) = test_registry();

        let err = registry
            .launch("audit", Some("gpt-4o-mini"), None, "work".to_string())
            .await
            .expect_err("a wire identifier is not a model name");
        assert_eq!(
            err,
            "no usable model named \"gpt-4o-mini\"; available: fast, smart"
        );
        assert!(
            registry.lock().is_empty(),
            "a refused launch must register nothing"
        );
        assert!(
            registry.spawned_lock().is_empty(),
            "a refused launch must spawn no task"
        );

        registry
            .launch("audit", Some("fast"), None, "work".to_string())
            .await
            .expect("the handle is still free for a corrected retry");
    }

    /// Attribution, transcript half: the header names the model a launch asked
    /// for, and an inherited launch adds no bytes at all. The trace and tool
    /// result are the other half, pinned in `builtin_tool`.
    #[tokio::test(start_paused = true)]
    async fn the_transcript_header_names_the_model() {
        let (registry, log_dir) = test_registry();
        registry
            .launch("audit", Some("fast"), None, "work".to_string())
            .await
            .expect("launch");
        registry.get_result("audit").await.expect("round fails");

        let text = tokio::fs::read_to_string(log_dir.path().join("subagent-audit.log"))
            .await
            .expect("transcript exists");
        assert!(
            text.starts_with(
                "=== subagent audit (model: fast / provider: openai / gpt-4o-mini) ===\n"
            ),
            "got: {text}"
        );

        let (inherited, inherited_log) = test_registry();
        inherited
            .launch("plain", None, None, "work".to_string())
            .await
            .expect("launch");
        inherited.get_result("plain").await.expect("round fails");

        let text = tokio::fs::read_to_string(inherited_log.path().join("subagent-plain.log"))
            .await
            .expect("transcript exists");
        assert!(
            !text.contains("model:"),
            "an inherited launch must add no header bytes: {text}"
        );
    }

    /// The same header, launched under an alias: the first slot shows the hop,
    /// while the provider and identifier still describe the row that ran.
    #[tokio::test(start_paused = true)]
    async fn the_transcript_header_shows_the_alias_hop() {
        let (registry, log_dir) = registry_with(
            fixtures::alias_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        registry
            .launch("audit", Some("cheap"), None, "work".to_string())
            .await
            .expect("launch");
        registry.get_result("audit").await.expect("round fails");

        let text = tokio::fs::read_to_string(log_dir.path().join("subagent-audit.log"))
            .await
            .expect("transcript exists");
        assert!(
            text.starts_with(
                "=== subagent audit (model: cheap -> fast / provider: openai / gpt-4o-mini) ===\n"
            ),
            "got: {text}"
        );
    }

    /// An alias is usable when *any* candidate is, which is what lets one name
    /// span an in-process model and a hosted one. Naming `onprem` directly is
    /// refused in this build; naming the alias that lists it is not.
    #[cfg(not(feature = "local-llm"))]
    #[test]
    fn an_alias_over_a_local_and_a_remote_candidate_is_usable_without_local_llm() {
        let cfg = fixtures::mixed_alias_config();
        let usable = usable_model_names(&cfg);
        assert!(
            usable.contains(&"either".to_string()),
            "the alias falls through to its hosted candidate: {usable:?}"
        );
        assert!(
            !usable.contains(&"onprem".to_string()),
            "naming the in-process model directly is still not offered: {usable:?}"
        );
    }

    /// With the feature on, the same alias prefers its first candidate.
    #[cfg(feature = "local-llm")]
    #[test]
    fn an_alias_over_a_local_and_a_remote_candidate_prefers_the_local_one() {
        let cfg = fixtures::mixed_alias_config();
        assert!(usable_model_names(&cfg).contains(&"either".to_string()));
    }

    /// An unset api-key variable is a guaranteed failure, so a model behind one
    /// is not advertised. This is the same predicate the resolver selects alias
    /// candidates with, which is why the two cannot drift.
    #[test]
    fn a_model_whose_key_is_unset_is_not_advertised() {
        let mut cfg = fixtures::test_config();
        cfg.providers.insert(
            "keyless".to_string(),
            outrig::config::LlmProvider::openai(
                "http://127.0.0.1:9",
                outrig::config::ApiKeyRef::parse("${OUTRIG_TEST_SUBAGENT_NEVER_SET_KEY}")
                    .expect("api-key ref parses"),
                outrig::config::OpenAiOptions::new().with_request_timeout_secs(1),
            ),
        );
        let mut model = outrig::config::Model::new("keyless");
        model.identifier = Some("gpt-4o".to_string());
        cfg.models.insert("unreachable".to_string(), model);

        let usable = usable_model_names(&cfg);
        assert_eq!(
            usable,
            vec!["fast".to_string(), "smart".to_string()],
            "a model behind an unset key is not offered"
        );
    }

    /// A local model in a default build must fail with the feature-disabled
    /// error, not "unknown model" -- the remedy is a rebuild, and collapsing the
    /// two would hide that. The schema omits the name; the error path does not.
    #[cfg(not(feature = "local-llm"))]
    #[tokio::test(start_paused = true)]
    async fn local_model_in_default_build_fails_feature_disabled() {
        let (registry, _log_dir) = registry_with(
            local_model_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );

        assert!(
            !usable_model_names(registry.config()).contains(&"onprem".to_string()),
            "a name that cannot resolve must not be advertised"
        );
        let err = registry
            .launch("audit", Some("onprem"), None, "work".to_string())
            .await
            .expect_err("no local-llm feature");
        assert!(err.contains("local-llm"), "got: {err}");
        assert!(
            !err.contains("no usable model named"),
            "the feature-disabled cause must not be collapsed into unknown: {err}"
        );
    }

    /// Crossing provider *styles* needs no new dispatch: `RigAgent` is already
    /// a runtime-dispatched enum, so a hosted parent naming an in-process model
    /// just resolves to the mistralrs arm.
    ///
    /// Stops at resolution deliberately. Going on to `build_agent` would load
    /// multi-gigabyte weights (or try to fetch them), which no unit test can
    /// afford; what this pins down is that the resolution reaches the
    /// `Mistralrs` provider with weights attached, which is the only input the
    /// dispatch reads.
    #[cfg(feature = "local-llm")]
    #[tokio::test(start_paused = true)]
    async fn subagent_may_cross_provider_style() {
        let (registry, _log_dir) = registry_with(
            local_model_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        assert!(
            matches!(
                registry.ctx.resolved.provider(),
                crate::llm::ResolvedProvider::OpenAi { .. }
            ),
            "the parent is hosted"
        );

        let resolved = resolve_launch_model(&registry.ctx, Some("onprem")).expect("resolves");
        assert!(matches!(
            resolved.provider(),
            crate::llm::ResolvedProvider::Mistralrs
        ));
        assert!(
            resolved.model_weights().is_some(),
            "the mistralrs arm carries the weight spec build_agent loads from"
        );
        assert!(
            usable_model_names(registry.config()).contains(&"onprem".to_string()),
            "a local model is advertised in a local-llm build"
        );
    }

    /// Two subagents naming the same in-process model share one loaded engine:
    /// `LlmRegistry` lives on the context and is keyed by model name, so both
    /// launches reach one slot and only the first pays for the load.
    ///
    /// The engine is a local stub rather than a real `MistralrsModel`, which
    /// wraps a multi-gigabyte `MistralRs`: the sharing is a property of the
    /// registry's keying, and the key is what the two resolutions agree on. The
    /// context's own registry is asked for `is_loaded` too, since that is the
    /// guard on the cold-load announcement.
    #[cfg(feature = "local-llm")]
    #[tokio::test(start_paused = true)]
    async fn two_subagents_on_one_local_model_share_one_engine() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct Stub;

        let (registry, _log_dir) = registry_with(
            local_model_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        let first = resolve_launch_model(&registry.ctx, Some("onprem")).expect("resolves");
        let second = resolve_launch_model(&registry.ctx, Some("onprem")).expect("resolves");
        assert_eq!(
            first.model_name(),
            second.model_name(),
            "both launches must reach the registry under one key"
        );
        assert!(
            !registry.ctx.registry.is_loaded(first.model_name()),
            "nothing is loaded yet, so the first launch announces a cold load"
        );

        let engines: crate::llm::LlmRegistry<Stub> = crate::llm::LlmRegistry::new();
        let loads = AtomicUsize::new(0);
        let one = engines
            .get_or_init(first.model_name(), || async {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Stub)
            })
            .await
            .expect("first load");
        let two = engines
            .get_or_init(second.model_name(), || async {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Stub)
            })
            .await
            .expect("the second launch finds the slot");

        assert!(Arc::ptr_eq(&one, &two), "one engine, shared");
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "two subagents on one model must not load the weights twice"
        );
        assert!(
            engines.is_loaded(first.model_name()),
            "a loaded model must not be announced as cold again"
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
