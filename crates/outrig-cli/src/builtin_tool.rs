//! OutRig's own tools, exposed to the model as `outrig__<tool>`.
//!
//! These are the only tools that do not come from an MCP server. They reach
//! the model through the same [`sanitize_tool_name`](outrig::sanitize_tool_name)
//! path as MCP tools, so `outrig__subagent` sits beside `fs__read_file` and
//! `shell__exec` and needs no special handling on the model's side. The
//! `outrig` server name is reserved in config validation so nothing can shadow
//! them.
//!
//! The set is split by who may call it. The parent-side tools launch and
//! collect subagents; [`SetResultTool`] is how a subagent reports back, so only
//! a subagent gets it. The two are not exclusive -- a subagent under the depth
//! limit gets both and can launch subagents of its own. What terminates the
//! recursion is that the launch tools are withheld at the maximum depth,
//! leaving that subagent with nothing to launch with.

use std::sync::{Arc, OnceLock};

use rig::tool::{ToolContext, ToolErrorKind, ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::{ANTHROPIC_FALLBACK_MAX_TOKENS, EffectiveCeiling, TurnUndo};
use crate::rig_tool::truncate_for_llm;
use crate::session_tool::OutrigTool;
use crate::subagent::state::{GaveUp, Outcome, SubagentShared};
use crate::subagent::{Read, SubagentRegistry};

/// Wraps a failure as the model-visible tool error: its text is what the
/// model reads, so it is never redacted the way rig's `from_error` would.
pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T, ToolExecutionError> {
    Err(ToolExecutionError::new(ToolErrorKind::Other, message))
}

/// Every tool schema below advertises `additionalProperties: false`, and each
/// arg struct carries `deny_unknown_fields` to match. Letting the decoder be
/// laxer than the advertised contract is the same class of drift that made
/// `set_result` accept `{}` while its prose said otherwise. The arguments are
/// always an object by the time they get here: rig decodes the model's JSON,
/// and sends a call whose arguments are anything else to no tool.
pub(crate) fn parse_args<T: for<'de> Deserialize<'de>>(
    args: Value,
) -> Result<T, ToolExecutionError> {
    serde_json::from_value(args).map_err(|e| ToolExecutionError::invalid_args(e.to_string()))
}

pub(crate) fn name_of(tool: &str) -> String {
    outrig::sanitize_tool_name(outrig::RESERVED_SERVER, tool)
}

// ---------------------------------------------------------------- launch ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchArgs {
    name: String,
    /// Present in both schema shapes. When the schema omits the property --
    /// because only one model is usable -- a caller that sends it anyway still
    /// deserializes and gets the "no usable model named ..." refusal, which is
    /// the safe direction of drift: the hazard the standing comment names is a
    /// field the schema advertises and the decoder refuses.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    preamble: Option<String>,
    prompt: String,
}

/// `outrig__subagent`: start a subagent and return immediately.
#[derive(Clone)]
pub struct SubagentTool {
    registry: Arc<SubagentRegistry>,
    /// The models this build could reach, read straight into the schema's
    /// `enum`.
    usable_models: Vec<String>,
    /// The launching agent's own model, so "omit to use yours" is concrete.
    parent_model: String,
}

impl SubagentTool {
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        // Read from the registry's own context, once, at construction: the
        // schema has to be byte-stable across calls, since an unstable one
        // churns the parent's context for nothing. Doing it here rather than in
        // `parent_tools` also means the nested-launch path cannot describe a
        // different launching agent than the session path does.
        let usable_models = crate::subagent::usable_model_names(registry.config());
        let parent_model = registry.parent_model_name().to_string();
        Self {
            registry,
            usable_models,
            parent_model,
        }
    }
}

impl OutrigTool for SubagentTool {
    fn name(&self) -> String {
        name_of("subagent")
    }

    fn description(&self) -> String {
        "Launch a subagent to work on a task in the background, and return \
         immediately. The subagent shares your container, tools and workspace, \
         but starts with a fresh context: it sees only the preamble and prompt \
         you give it, so write them to stand alone. Launch several to work in \
         parallel. Collect a subagent's findings with outrig__get_result. You \
         are responsible for not giving concurrent subagents overlapping edits \
         to the same files."
            .to_string()
    }

    fn parameters(&self) -> Value {
        let mut properties = json!({
            "name": {
                "type": "string",
                "description": "Short kebab-case handle used to refer to this \
                                subagent later, e.g. \"audit-config\"."
            },
            "preamble": {
                "type": "string",
                "description": "Optional system prompt: the role or standing \
                                context this subagent should work under. Omit \
                                it when the prompt alone is enough."
            },
            "prompt": {
                "type": "string",
                "description": "The task, stated in full. The subagent cannot \
                                see your conversation."
            }
        });
        // A choice of one is not a choice, and the set always holds at least
        // the parent's own model -- so leave the property out entirely rather
        // than spend the parent's context advertising it.
        if self.usable_models.len() > 1 {
            properties["model"] = json!({
                "type": "string",
                "enum": self.usable_models,
                "description": format!(
                    "Which configured model this subagent runs under. Omit to use \
                     yours (\"{}\"). Delegating mechanical work -- grepping a tree, \
                     summarizing logs, checking whether a symbol is still used -- to \
                     a cheaper model keeps your own context for synthesis.",
                    self.parent_model
                )
            });
        }
        json!({
            "type": "object",
            "properties": properties,
            "required": ["name", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let args: LaunchArgs = parse_args(args)?;
        match self
            .registry
            .launch(
                &args.name,
                args.model.as_deref(),
                args.preamble,
                args.prompt,
            )
            .await
        {
            Ok(launched_as) => {
                // Empty unless the launch named a model, so an inherited one
                // reads exactly as it always has. The name comes back from
                // the launch rather than from `args.model`, so an alias
                // shows the row it resolved to.
                let suffix = launched_as.map_or(String::new(), |label| {
                    format!(" (model: {})", label.display_name())
                });
                eprintln!("[outrig] subagent {} started{suffix}", args.name);
                Ok(ToolOutput::text(format!(
                    "subagent {:?} started{suffix}; collect it with outrig__get_result",
                    args.name
                )))
            }
            Err(e) => fail(e),
        }
    }
}

// ------------------------------------------------------------------ send ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendArgs {
    name: String,
    prompt: String,
}

/// `outrig__subagent_send`: give a subagent more work, whatever it is doing.
#[derive(Clone)]
pub struct SubagentSendTool {
    registry: Arc<SubagentRegistry>,
}

impl SubagentSendTool {
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        Self { registry }
    }
}

impl OutrigTool for SubagentSendTool {
    fn name(&self) -> String {
        name_of("subagent_send")
    }

    fn description(&self) -> String {
        "Send a prompt to a subagent you already launched -- to follow up on a \
         result, or to redirect one mid-task. Works whether it is idle or still \
         working: an idle subagent starts a new round, a busy one sees your \
         message at its next step. It keeps its history, so you can refer back \
         to what it already did."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The subagent's handle." },
                "prompt": { "type": "string", "description": "What it should do next." }
            },
            "required": ["name", "prompt"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let args: SendArgs = parse_args(args)?;
        match self.registry.send(&args.name, args.prompt) {
            Ok(what) => Ok(ToolOutput::text(format!(
                "delivered to {:?}: {what}",
                args.name
            ))),
            Err(e) => fail(e),
        }
    }
}

// ------------------------------------------------------------------ wait ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    names: Vec<String>,
    #[serde(default)]
    min_count: Option<usize>,
}

/// `outrig__wait_results`: block until enough subagents have something to
/// collect, and say which. Carries no payloads on purpose.
#[derive(Clone)]
pub struct WaitResultsTool {
    registry: Arc<SubagentRegistry>,
}

impl WaitResultsTool {
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        Self { registry }
    }
}

impl OutrigTool for WaitResultsTool {
    fn name(&self) -> String {
        name_of("wait_results")
    }

    fn description(&self) -> String {
        "Wait until at least min_count of the named subagents have something to \
         collect, then return which ones. Returns names only, not their \
         findings -- call outrig__get_result per subagent to read those, so one \
         call cannot flood your context. Use min_count to react to whichever \
         finishes first. A ready subagent stays ready until you collect it, so \
         this returns it again at once. One whose result you collected counts \
         again only when it reports again; if it is idle, it is not waited on, \
         and a call naming only such subagents fails."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Handles of the subagents to wait on."
                },
                "min_count": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "How many must be ready before returning. \
                                    Defaults to all of them; idle ones \
                                    with nothing new do not count."
                }
            },
            "required": ["names"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let args: WaitArgs = parse_args(args)?;
        let min_count = args.min_count.unwrap_or(args.names.len()).max(1);
        if min_count > args.names.len() {
            return fail(format!(
                "min_count {min_count} exceeds the {} name(s) given",
                args.names.len()
            ));
        }
        match self.registry.wait_results(&args.names, min_count).await {
            // As text, not a JSON block: the wire reads as it always has.
            Ok(ready) => Ok(ToolOutput::text(json!({ "ready": ready }).to_string())),
            Err(e) => fail(e),
        }
    }
}

// ------------------------------------------------------------------- get ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    name: String,
}

/// `outrig__get_result`: block until this subagent has something new, return
/// it, and advance the parent's read position past it -- for good once the
/// turn keeps the result, and back again through [`TurnUndo`] if it does not.
#[derive(Clone)]
pub struct GetResultTool {
    registry: Arc<SubagentRegistry>,
    result_cap_bytes: usize,
}

impl GetResultTool {
    pub fn new(registry: Arc<SubagentRegistry>, result_cap_bytes: usize) -> Self {
        Self {
            registry,
            result_cap_bytes,
        }
    }

    /// The call, with the turn it is part of when there is one.
    async fn get(
        &self,
        args: Value,
        turn: Option<Arc<TurnUndo>>,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let args: GetArgs = parse_args(args)?;
        let Read { outcome, claim } = match self.registry.read(&args.name).await {
            Ok(read) => read,
            Err(e) => return fail(e),
        };
        // Recorded whichever way the outcome reaches the model: an error moved
        // the position just as a result did. Weak, so an undo still pending
        // cannot keep the registry -- and the session tools it holds -- alive.
        if let Some(turn) = turn {
            let registry = Arc::downgrade(&self.registry);
            turn.record(move || {
                if let Some(registry) = registry.upgrade() {
                    registry.put_back(claim);
                }
            });
        }
        match outcome {
            Outcome::Result(text) => Ok(ToolOutput::text(truncate_for_llm(
                &text,
                self.result_cap_bytes,
            ))),
            Outcome::Error(text) => fail(truncate_for_llm(&text, self.result_cap_bytes)),
        }
    }
}

impl OutrigTool for GetResultTool {
    fn name(&self) -> String {
        name_of("get_result")
    }

    fn description(&self) -> String {
        "Read one subagent's latest findings, waiting if it has not reported \
         yet. Each result is returned once: calling again waits for the next \
         one, so follow up with outrig__subagent_send if you want more. A \
         subagent that stopped without reporting comes back as an error -- \
         send it a prompt to get it going again."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The subagent's handle." }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    /// A turn hands each call its context, and [`TurnUndo::of`] finds the
    /// turn in it; a call outside one is part of no turn.
    async fn call(
        &self,
        context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        self.get(args, TurnUndo::of(context)).await
    }
}

// --------------------------------------------------------------- release ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseArgs {
    names: Vec<String>,
}

/// `outrig__subagent_release`: end subagents and free their handles.
#[derive(Clone)]
pub struct SubagentReleaseTool {
    registry: Arc<SubagentRegistry>,
}

impl SubagentReleaseTool {
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        Self { registry }
    }
}

impl OutrigTool for SubagentReleaseTool {
    fn name(&self) -> String {
        name_of("subagent_release")
    }

    fn description(&self) -> String {
        "Stop subagents you are done with and free their handles for reuse. \
         Subagents you never release keep their history and stay available \
         until the session ends."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "names": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Handles of the subagents to stop."
                }
            },
            "required": ["names"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        let args: ReleaseArgs = parse_args(args)?;
        match self.registry.release(&args.names) {
            Ok(released) => Ok(ToolOutput::text(format!(
                "released: {}",
                released.join(", ")
            ))),
            Err(e) => fail(e),
        }
    }
}

// ------------------------------------------------------------ set_result ---

/// Which kind of outcome the body carries.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum ResultStatus {
    Result,
    Error,
}

/// Both fields are required, and that is the whole point.
///
/// This started as `{result}` xor `{error}`, two optional strings. Models --
/// including current frontier ones -- routinely called it as `{}`, which that
/// schema *permits*: neither field was required, so an empty call was legal
/// and the rejection could only come from the runtime, after the call was
/// already spent. Adding `oneOf` documented the constraint without enforcing
/// it; `oneOf` is validation vocabulary that decoding does not honor and that
/// provider bridges (Bedrock's tool-schema subset in particular) may drop.
///
/// `required` is the one constraint that is both honored by providers and
/// attended to by models, and an exclusive choice between two optional fields
/// cannot use it. Moving the choice into an enum makes both fields mandatory,
/// so the empty call is rejected before it is ever generated.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetResultArgs {
    status: ResultStatus,
    body: String,
}

/// A `status` with no `body` is the signature of a reply cut off at the
/// output-token ceiling: the fields are generated in schema order, so a
/// truncated tool call keeps the short leading field and loses the long
/// trailing one.
///
/// Serde's own "missing field `body`" is accurate and useless here -- it gives
/// the model no reason to do anything differently, so it regenerates the same
/// oversized body and fails identically. Naming the likely cause turns that
/// loop into a recoverable one, because shortening the report actually works.
fn looks_truncated(args: &Value) -> bool {
    let Value::Object(map) = args else {
        return false;
    };
    map.contains_key("status") && !map.contains_key("body")
}

/// How many times a subagent is asked to retry a report that arrived without a
/// `body` before OutRig gives up on its behalf.
///
/// Two, because the retry is a long shot and each one costs a model call. The
/// ceiling that cut the report off does not move between attempts, so a model
/// that could have fit the report in `body` would have done it when first
/// asked; past that the same oversized body is regenerated and fails
/// identically. Left unbounded this ran until the per-turn tool-call max --
/// roughly fifty model calls spent to learn what the first failure already
/// said.
const TRUNCATION_RETRY_BUDGET: u32 = 2;

/// The first ask: name the likely cause, because serde's own "missing field
/// `body`" gives the model no reason to do anything differently.
const SHORTEN_THE_REPORT: &str = "\
    nothing was recorded: `status` arrived but `body` did not. That usually \
    means the reply hit the output token limit part-way through the report. \
    Call again with a shorter `body` -- summarize your findings rather than \
    quoting at length. If it genuinely cannot be shortened, say so via \
    `status: \"error\"` with a short `body`.";

/// [`SHORTEN_THE_REPORT`], for a round whose report already stands (#521).
/// Offering `status: "error"` here would invite a model to publish a short
/// error over the report, which the inbox keeps only the latest of.
const SHORTEN_A_REVISION: &str = "\
    nothing was recorded by this call: `status` arrived but `body` did not. \
    That usually means the reply hit the output token limit part-way through. \
    The report you recorded earlier stands, and the agent that launched you \
    will read it. If the revision matters, call again with a shorter `body`; \
    otherwise stop calling outrig__set_result.";

/// What every truncated call gets once the round has given up and told the
/// parent so.
///
/// Deliberately a *failure* rather than a success. Returning `Ok` here read as
/// "call handled" to the repeat breaker in [`crate::llm::RepeatTracker`], which
/// clears its count on any success -- so the one backstop that could have ended
/// the round was disarmed by the give-up it was meant to catch. Failing keeps
/// identical calls accumulating until the breaker stops the round.
const ALREADY_REPORTED: &str = "\
    nothing further was recorded. The agent that launched you has already been \
    told that your reply is being cut off at its output-token limit, so this \
    report cannot be delivered. Stop calling outrig__set_result.";

/// [`ALREADY_REPORTED`], for a round that gave up after it had reported: that
/// report stands, and the agent that launched it was told nothing (#350).
const REPORT_STANDS: &str = "\
    nothing further was recorded. This report is being cut off at your \
    output-token limit, so it cannot be delivered; the agent that launched you \
    keeps the report you recorded earlier. Stop calling outrig__set_result.";

/// What a truncated call gets once the round has given up, by how it did.
fn refusal(how: GaveUp) -> &'static str {
    match how {
        GaveUp::Told => ALREADY_REPORTED,
        GaveUp::KeptReport => REPORT_STANDS,
    }
}

/// The last ask. Retrying the report is off the table by now, so this asks for
/// the one thing that still fits: a short error the parent can act on.
const SEND_A_SHORT_ERROR: &str = "\
    still nothing recorded: `body` was missing again, so the report is being \
    cut off before it is written. Do not attempt the full report a third time \
    -- it will be cut off in the same place. Call once more with `status: \
    \"error\"` and a one-sentence `body` naming what you found and that the \
    report did not fit.";

/// [`SEND_A_SHORT_ERROR`], for a round whose report already stands (#521).
const LEAVE_THE_REPORT: &str = "\
    still nothing recorded: `body` was missing again, so the revision is being \
    cut off before it is written. The report you recorded earlier stands, and \
    the agent that launched you will read it. Do not attempt the longer \
    revision again -- it will be cut off in the same place. Stop calling \
    outrig__set_result.";

/// `outrig__set_result`: the subagent-side tool. Publishes into the inbox the
/// parent reads, and does **not** end the round -- the subagent may keep
/// working and publish again, in which case the later value wins.
#[derive(Clone)]
pub struct SetResultTool {
    shared: Arc<SubagentShared>,
    /// This subagent's handle, for the trace prefix, matching the shape the
    /// agent loop's own traces use.
    label: Arc<str>,
    /// The agent whose turns hit the ceiling. Carried only to name the fix
    /// in the one operator-facing warning below. `None` for an agentless
    /// session, which has no `[agents.<name>]` table to point the operator at.
    agent_name: Option<Arc<str>>,
    /// The ceilings this subagent's candidates send, which may differ from
    /// the configured ones. Filled once `build_agent` has computed them; this
    /// tool is one of its inputs, so it cannot be handed them directly.
    ceilings: Arc<OnceLock<Vec<EffectiveCeiling>>>,
}

/// What to tell the operator about a cut-off report, from the ceilings the
/// subagent's candidates actually send (#258). Empty means the cell was never
/// filled, which is told as an unset value.
fn ceiling_remedy(scope: &str, agent_name: Option<&str>, ceilings: &[EffectiveCeiling]) -> String {
    let ceilings = match ceilings {
        [] => return single_ceiling_remedy(scope, agent_name, &EffectiveCeiling::default()),
        [only] => return single_ceiling_remedy(scope, agent_name, only),
        many => many,
    };
    // Which candidate wrote the cut-off report is not knowable from here, so
    // name every candidate's number rather than the first one's.
    let sent = ceilings
        .iter()
        .map(|c| match c.sent {
            Some(n) => format!("{} {n}", c.model_name),
            None => format!("{} (none sent)", c.model_name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    // An agent's `max-tokens` overrides every candidate's model row, so where
    // there is an agent it is the knob that always takes effect; editing a
    // `[models.<name>]` row would do nothing when the agent sets one.
    let knob = match agent_name {
        Some(agent) => format!(
            "set [agents.{agent}].max-tokens, which overrides each candidate's \
             [models.<name>].max-tokens"
        ),
        None => "raise the answering candidate's [models.<name>].max-tokens".to_string(),
    };
    format!(
        "this alias's candidates send max-tokens {sent}; whichever answered hit its own \
         -- {knob}, or ask for a shorter or split report"
    )
}

fn single_ceiling_remedy(
    scope: &str,
    agent_name: Option<&str>,
    ceiling: &EffectiveCeiling,
) -> String {
    match (ceiling.configured, ceiling.sent) {
        // Lowered to what the identifier serves: raising the setting cannot
        // help, so do not say to.
        (Some(configured), Some(sent)) if configured > sent => format!(
            "max-tokens for {scope} is {configured}, but this model serves at most {sent}, so \
             {sent} was sent -- raising it cannot help; ask for a shorter or split report"
        ),
        (Some(limit), _) => {
            format!("the max-tokens in effect for {scope} is {limit} -- raise it")
        }
        // Nothing set it, but outrig or rig chose a number: name it.
        (None, Some(sent)) if u64::from(sent) == ANTHROPIC_FALLBACK_MAX_TOKENS => {
            format!(
                "max-tokens is unset, so outrig's fallback of {sent} was sent -- set \
                 max-tokens explicitly{}",
                where_to_set(agent_name)
            )
        }
        (None, Some(sent)) => format!(
            "max-tokens is unset, so the model's published maximum of {sent} was sent -- \
             raising it cannot help; ask for a shorter or split report"
        ),
        // Nothing set it anywhere, so there is no ambiguity about which
        // table to name -- except that an agentless session has no
        // `[agents.<name>]` table, and its only knob is the model's.
        (None, None) => match agent_name {
            Some(agent) => format!(
                "[agents.{agent}].max-tokens is unset, so the provider's default \
                 applies -- set it explicitly"
            ),
            None => "max-tokens is unset, so the provider's default applies -- set \
                     [models.<name>].max-tokens explicitly"
                .to_string(),
        },
    }
}

/// ` in [agents.<name>]` or the model table, for the end of a sentence.
fn where_to_set(agent_name: Option<&str>) -> String {
    match agent_name {
        Some(agent) => format!(" in [agents.{agent}]"),
        None => " in [models.<name>]".to_string(),
    }
}

impl SetResultTool {
    pub fn new(
        shared: Arc<SubagentShared>,
        label: &str,
        agent_name: Option<&str>,
        ceilings: Arc<OnceLock<Vec<EffectiveCeiling>>>,
    ) -> Self {
        Self {
            shared,
            label: Arc::from(label),
            agent_name: agent_name.map(Arc::from),
            ceilings,
        }
    }

    /// Tell the operator what the model cannot: that this is a configuration
    /// ceiling, and which knob moves it.
    ///
    /// The subagent's own retries are model-facing and say nothing useful to a
    /// human reading the log; without this the only trace of the cause is a
    /// wall of identical tool-call errors.
    fn warn_about_the_ceiling(&self) {
        // The effective ceiling can come from the agent or from the model it
        // uses, so name the knob without claiming which file set it.
        let scope = match &self.agent_name {
            Some(agent) => format!("agent {agent}"),
            None => "this session".to_string(),
        };
        let remedy = ceiling_remedy(
            &scope,
            self.agent_name.as_deref(),
            self.ceilings.get().map_or(&[], Vec::as_slice),
        );
        eprintln!(
            "[outrig]   [{}] set_result was cut off before `body`: the model's \
             output-token ceiling is too low for this report. {remedy}.",
            self.label
        );
    }
}

impl OutrigTool for SetResultTool {
    fn name(&self) -> String {
        name_of("set_result")
    }

    fn description(&self) -> String {
        "Report back to the agent that launched you. Set status to \"result\" \
         when you have findings and \"error\" when you could not finish, and put \
         the whole report in body. That body is the only thing that agent sees, \
         so make it self-contained: it cannot read your conversation. You may \
         keep working afterwards and call this again to revise; the most recent \
         call is what it reads."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["result", "error"],
                    "description": "\"result\" if you have findings to report, \
                                    \"error\" if you could not finish."
                },
                "body": {
                    "type": "string",
                    "minLength": 1,
                    "description": "The full report: your findings, or why you \
                                    could not finish. This is all the agent that \
                                    launched you will see."
                }
            },
            "required": ["status", "body"],
            "additionalProperties": false
        })
    }

    async fn call(
        &self,
        _context: &mut ToolContext,
        args: Value,
    ) -> Result<ToolOutput, ToolExecutionError> {
        if looks_truncated(&args) {
            // Remember it on the subagent too: if the round goes on to end
            // without publishing, the parent gets the cause rather than a
            // bare "it stopped".
            let attempt = self.shared.note_truncated_attempt();
            // Checked before the attempt count, and latched rather than
            // derived from it, so that everything below happens exactly
            // once per round however many truncated calls keep arriving.
            if let Some(how) = self.shared.gave_up() {
                return fail(refusal(how));
            }
            return match attempt {
                1 => {
                    self.warn_about_the_ceiling();
                    fail(if self.shared.report_stands() {
                        SHORTEN_A_REVISION
                    } else {
                        SHORTEN_THE_REPORT
                    })
                }
                n if n <= TRUNCATION_RETRY_BUDGET => fail(if self.shared.report_stands() {
                    LEAVE_THE_REPORT
                } else {
                    SEND_A_SHORT_ERROR
                }),
                // Out of retries. Publishing on the subagent's behalf is
                // what makes this converge: `end_round` sees a round that
                // published, and a parent blocked in `get_result` wakes now
                // with the cause instead of once the tool-call budget has
                // drained into the same failing call. A round that reported
                // already keeps its report instead (#350).
                _ => fail(refusal(self.shared.give_up())),
            };
        }
        let args: SetResultArgs = parse_args(args)?;
        // `required` keeps the fields present; it cannot keep `body`
        // meaningful, and publishing an empty report would leave the parent
        // with nothing while looking like success.
        if args.body.trim().is_empty() {
            return fail(
                "nothing was recorded: `body` was empty. Call again with the \
                 full report in `body`.",
            );
        }
        // A body that arrived whole is the only evidence that the ceiling
        // is survivable, so it -- and not merely publishing -- is what
        // restores the retry budget.
        self.shared.note_report_fit();
        match args.status {
            ResultStatus::Result => {
                self.shared.publish(Outcome::Result(args.body));
                Ok(ToolOutput::text("result recorded"))
            }
            ResultStatus::Error => {
                self.shared.publish(Outcome::Error(args.body));
                Ok(ToolOutput::text("error recorded"))
            }
        }
    }
}

/// The parent-side set, in the order they are advertised.
pub fn parent_tools(
    registry: Arc<SubagentRegistry>,
    result_cap_bytes: usize,
) -> Vec<crate::session_tool::SessionTool> {
    use crate::session_tool::SessionTool;
    vec![
        SessionTool::new(SubagentTool::new(registry.clone())),
        SessionTool::new(SubagentSendTool::new(registry.clone())),
        SessionTool::new(WaitResultsTool::new(registry.clone())),
        SessionTool::new(GetResultTool::new(registry.clone(), result_cap_bytes)),
        SessionTool::new(SubagentReleaseTool::new(registry)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::state::TRUNCATED_REPORT;

    /// The argument text `json`, decoded as rig decodes a call's arguments.
    fn json_args(json: &str) -> Value {
        serde_json::from_str(json).expect("test arguments are JSON")
    }

    /// Run `tool` on the argument text `json` outside a turn, and render
    /// what it returned.
    async fn call<T: OutrigTool>(tool: &T, json: &str) -> Result<String, ToolExecutionError> {
        tool.call(&mut ToolContext::new(), json_args(json))
            .await
            .map(|output| output.render())
    }

    /// The built-ins must look exactly like MCP tools to the model, which
    /// means going through the same `<server>__<tool>` sanitizer.
    #[test]
    fn builtin_names_use_the_reserved_prefix() {
        assert_eq!(name_of("subagent"), "outrig__subagent");
        assert_eq!(name_of("subagent_send"), "outrig__subagent_send");
        assert_eq!(name_of("wait_results"), "outrig__wait_results");
        assert_eq!(name_of("get_result"), "outrig__get_result");
        assert_eq!(name_of("subagent_release"), "outrig__subagent_release");
        assert_eq!(name_of("set_result"), "outrig__set_result");
    }

    #[test]
    fn launch_args_accept_an_omitted_preamble() {
        let args: LaunchArgs =
            parse_args(json_args(r#"{"name":"audit","prompt":"check the config"}"#))
                .expect("parses");
        assert_eq!(args.name, "audit");
        assert!(args.preamble.is_none());
        assert!(args.model.is_none(), "an omitted model means \"mine\"");
    }

    /// The `outrig__subagent` schema as `parent_tools` builds it, over a
    /// registry whose context is the fixture session's.
    fn subagent_schema(cfg: outrig::config::Config) -> Value {
        let (registry, log_dir) = crate::subagent::fixtures::registry_with(
            cfg,
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        let tools = parent_tools(Arc::new(registry), 4096);
        let schema = tools
            .iter()
            .find(|tool| tool.name() == "outrig__subagent")
            .expect("the launch tool is advertised")
            .parameters();
        drop(log_dir);
        schema
    }

    /// The enum is what the model reliably attends to, so it must list exactly
    /// the names a launch would accept -- and in a stable order, since an
    /// unstable schema churns the parent's context for nothing.
    #[test]
    fn subagent_schema_model_enum_lists_usable_models_only() {
        let mut cfg = crate::subagent::fixtures::test_config();
        // Dangling provider: resolution would fail with UnknownProvider, so the
        // name is not offered.
        cfg.models
            .insert("orphan".to_string(), outrig::config::Model::new("nowhere"));

        let schema = subagent_schema(cfg.clone());
        assert_eq!(
            schema["properties"]["model"]["enum"],
            json!(["fast", "smart"]),
            "got: {schema}"
        );
        assert_eq!(
            schema,
            subagent_schema(cfg),
            "the schema must be byte-stable across calls"
        );
    }

    /// A choice of one is not a choice: the set always holds the parent's own
    /// model, so a lone entry means the property earns nothing.
    #[test]
    fn subagent_schema_omits_model_when_only_one_usable() {
        let schema = subagent_schema(crate::subagent::fixtures::test_config_single());
        assert!(schema["properties"].get("model").is_none(), "got: {schema}");
        assert_eq!(schema["required"], json!(["name", "prompt"]));
    }

    /// "Omit to use yours" is only actionable if it names the model concretely.
    #[test]
    fn subagent_schema_model_description_names_parent_model() {
        let schema = subagent_schema(crate::subagent::fixtures::test_config());
        let description = schema["properties"]["model"]["description"]
            .as_str()
            .expect("the model property carries a description");
        assert!(
            description.contains("Omit to use yours (\"smart\")"),
            "got: {description}"
        );
    }

    /// The launch tool over the fixture session, plus the temp dir it writes
    /// transcripts into -- held by the caller so it outlives the launch.
    fn subagent_tool() -> (SubagentTool, tempfile::TempDir) {
        let (registry, log_dir) = crate::subagent::fixtures::registry_with(
            crate::subagent::fixtures::test_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        (SubagentTool::new(Arc::new(registry)), log_dir)
    }

    /// The result string is what the parent model reads back, so it is where
    /// "did I get the model I asked for" gets answered -- inferring it from the
    /// absence of an error is not the same thing. The stderr trace carries the
    /// same suffix from the same value.
    #[tokio::test(start_paused = true)]
    async fn a_launch_that_named_a_model_says_so_in_its_result() {
        let (tool, _log_dir) = subagent_tool();

        let result = call(
            &tool,
            r#"{"name":"audit","prompt":"check the config","model":"fast"}"#,
        )
        .await
        .expect("launch succeeds -- the client is built offline");
        assert_eq!(
            result,
            "subagent \"audit\" started (model: fast); collect it with outrig__get_result"
        );
    }

    /// Launched under an alias, the result names the hop rather than echoing
    /// the argument back. The parent asked for `cheap` and needs to know which
    /// row that turned out to be -- the alias could be repointed tomorrow.
    #[tokio::test(start_paused = true)]
    async fn a_launch_under_an_alias_names_the_hop_in_its_result() {
        let (registry, _log_dir) = crate::subagent::fixtures::registry_with(
            crate::subagent::fixtures::alias_config(),
            2,
            outrig::config::DEFAULT_SUBAGENT_DEPTH_MAX,
        );
        let tool = SubagentTool::new(Arc::new(registry));

        let result = call(
            &tool,
            r#"{"name":"audit","prompt":"check the config","model":"cheap"}"#,
        )
        .await
        .expect("launch succeeds -- the client is built offline");
        assert_eq!(
            result,
            "subagent \"audit\" started (model: cheap -> fast); collect it with outrig__get_result"
        );
    }

    /// An alias is a model name, so it is offered like any other -- which is
    /// what lets a user have intent-named models without duplicating a row to
    /// get them.
    #[test]
    fn subagent_schema_enum_lists_alias_names() {
        let schema = subagent_schema(crate::subagent::fixtures::alias_config());
        assert_eq!(
            schema["properties"]["model"]["enum"],
            json!(["cheap", "fast", "smart"]),
            "got: {schema}"
        );
    }

    /// The other half: an inherited launch must read byte-for-byte as it did
    /// before the argument existed, which only a literal comparison pins down.
    #[tokio::test(start_paused = true)]
    async fn an_inherited_launch_reads_exactly_as_it_did_before() {
        let (tool, _log_dir) = subagent_tool();

        let result = call(&tool, r#"{"name":"audit","prompt":"check the config"}"#)
            .await
            .expect("launch succeeds");
        assert_eq!(
            result,
            "subagent \"audit\" started; collect it with outrig__get_result"
        );
    }

    #[test]
    fn wait_args_accept_an_omitted_min_count() {
        let args: WaitArgs = parse_args(json_args(r#"{"names":["a","b"]}"#)).expect("parses");
        assert_eq!(args.min_count, None);
    }

    /// Regression, and the reason this tool's shape is what it is: the schema
    /// began as two optional strings, so `{}` was legal and frontier models
    /// made that call constantly. `required` is the constraint that actually
    /// stops it -- if these fields ever become optional again, the empty call
    /// comes back.
    /// A tool over a fresh inbox with a round already open, which is the state
    /// every `set_result` call actually arrives in.
    fn set_result_tool() -> (SetResultTool, Arc<SubagentShared>) {
        let shared = Arc::new(SubagentShared::new());
        shared.begin_round();
        (
            SetResultTool::new(shared.clone(), "audit", Some("primary"), Arc::default()),
            shared,
        )
    }

    fn sent(model: &str, configured: Option<u32>, sent: Option<u32>) -> EffectiveCeiling {
        EffectiveCeiling {
            model_name: model.to_string(),
            configured,
            sent,
        }
    }

    /// A configured value above what the model serves is lowered, so the
    /// remedy names what was sent and does not tell the operator to raise it
    /// (#258).
    #[test]
    fn a_capped_ceiling_names_what_was_sent_and_does_not_say_raise_it() {
        let remedy = ceiling_remedy(
            "agent primary",
            Some("primary"),
            &[sent("sonnet", Some(128_000), Some(64_000))],
        );
        assert!(
            remedy.contains("128000") && remedy.contains("64000"),
            "{remedy}"
        );
        assert!(!remedy.contains("raise it "), "{remedy}");
        assert!(remedy.contains("cannot help"), "{remedy}");
    }

    /// An unset value that outrig or rig filled in is named, not hidden
    /// behind "the provider's default".
    #[test]
    fn an_unset_ceiling_names_the_number_that_was_chosen() {
        let published = ceiling_remedy(
            "agent primary",
            Some("primary"),
            &[sent("sonnet", None, Some(64_000))],
        );
        assert!(published.contains("64000"), "{published}");
        assert!(!published.contains("provider's default"), "{published}");
        assert!(published.contains("cannot help"), "{published}");

        let fallback = ceiling_remedy(
            "agent primary",
            Some("primary"),
            &[sent("odd", None, Some(32_768))],
        );
        assert!(fallback.contains("32768"), "{fallback}");
        assert!(!fallback.contains("provider's default"), "{fallback}");
        assert!(fallback.contains("[agents.primary]"), "{fallback}");
    }

    /// Nothing is sent on the OpenAI arm when nothing is configured, and a
    /// configured value is sent as is: both keep their wording.
    #[test]
    fn an_openai_style_ceiling_keeps_its_wording() {
        assert_eq!(
            ceiling_remedy("agent a", Some("a"), &[sent("m", Some(4096), Some(4096))]),
            "the max-tokens in effect for agent a is 4096 -- raise it",
        );
        assert_eq!(
            ceiling_remedy("agent a", Some("a"), &[sent("m", None, None)]),
            "[agents.a].max-tokens is unset, so the provider's default applies -- set it \
             explicitly",
        );
        assert_eq!(
            ceiling_remedy("this session", None, &[sent("m", None, None)]),
            "max-tokens is unset, so the provider's default applies -- set \
             [models.<name>].max-tokens explicitly",
        );
    }

    /// A chain names every candidate's number: which one wrote the report
    /// is not known here.
    #[test]
    fn a_chain_names_each_candidates_ceiling() {
        let remedy = ceiling_remedy(
            "agent primary",
            Some("primary"),
            &[
                sent("head-model", Some(4096), Some(4096)),
                sent("next-model", Some(16_384), Some(16_384)),
            ],
        );
        assert!(remedy.contains("head-model 4096"), "{remedy}");
        assert!(remedy.contains("next-model 16384"), "{remedy}");
        assert!(remedy.contains("[agents.primary].max-tokens"), "{remedy}");

        let agentless = ceiling_remedy(
            "this session",
            None,
            &[sent("a", Some(1), Some(1)), sent("b", Some(2), Some(2))],
        );
        assert!(
            agentless.contains("[models.<name>].max-tokens"),
            "{agentless}"
        );
        assert!(!agentless.contains("[agents."), "{agentless}");
    }

    /// The truncated shape, as it arrives from the provider: `status` present
    /// because it generates first, `body` lost to the ceiling.
    async fn truncated_call(tool: &SetResultTool) -> Result<String, ToolExecutionError> {
        call(tool, r#"{"status":"result"}"#).await
    }

    #[test]
    fn set_result_schema_requires_both_fields() {
        let (tool, _shared) = set_result_tool();
        let schema = tool.parameters();

        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("schema marks fields required")
            .iter()
            .map(|v| v.as_str().expect("field name"))
            .collect();
        assert_eq!(required, vec!["status", "body"]);

        let statuses: Vec<&str> = schema["properties"]["status"]["enum"]
            .as_array()
            .expect("status is an enum")
            .iter()
            .map(|v| v.as_str().expect("variant"))
            .collect();
        assert_eq!(statuses, vec!["result", "error"]);
    }

    /// An empty call now fails at deserialization -- `status` and `body` are
    /// not `Option` -- which reaches the model as `invalid_args` rather than a
    /// generic tool error.
    #[test]
    fn set_result_args_reject_an_empty_call() {
        assert!(parse_args::<SetResultArgs>(json_args("{}")).is_err());
        assert!(parse_args::<SetResultArgs>(json_args(r#"{"status":"result"}"#)).is_err());
        assert!(parse_args::<SetResultArgs>(json_args(r#"{"body":"a"}"#)).is_err());
        assert!(
            parse_args::<SetResultArgs>(json_args(r#"{"status":"maybe","body":"a"}"#)).is_err()
        );
    }

    #[tokio::test]
    async fn set_result_publishes_under_the_status_it_was_given() {
        let (tool, shared) = set_result_tool();

        call(&tool, r#"{"status":"result","body":"found it"}"#)
            .await
            .expect("a result publishes");
        assert_eq!(
            shared.snapshot().outcome,
            Some(Outcome::Result("found it".to_string()))
        );

        call(&tool, r#"{"status":"error","body":"could not finish"}"#)
            .await
            .expect("an error publishes");
        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.outcome,
            Some(Outcome::Error("could not finish".to_string()))
        );
        assert_eq!(snapshot.version, 2, "each call bumps the version");
    }

    /// The failure seen in practice: `status` generated, `body` lost to the
    /// output-token ceiling. The message has to suggest shortening, or the
    /// model regenerates the same oversized body forever.
    #[tokio::test]
    async fn a_missing_body_is_reported_as_probable_truncation() {
        let (tool, shared) = set_result_tool();

        let err = truncated_call(&tool)
            .await
            .expect_err("status without body");
        let message = err.to_string();
        assert!(message.contains("token limit"), "got: {message}");
        assert!(message.contains("shorter"), "got: {message}");
        assert_eq!(shared.snapshot().version, 0, "nothing should publish");
        assert!(
            shared.snapshot().silent_cause.is_some(),
            "the cause must survive onto the subagent, so a round that ends \
             without publishing can explain itself to the parent"
        );
    }

    /// The second ask stops chasing the report. The ceiling has not moved, so
    /// asking again for a shorter version of the same thing is what produced
    /// the original loop; the remaining useful action is a short error.
    #[tokio::test]
    async fn the_second_truncated_attempt_asks_for_a_short_error_instead() {
        let (tool, shared) = set_result_tool();

        truncated_call(&tool).await.expect_err("first attempt");
        let err = truncated_call(&tool).await.expect_err("second attempt");

        let message = err.to_string();
        assert!(message.contains("one-sentence"), "got: {message}");
        assert!(
            message.contains("Do not attempt the full report"),
            "got: {message}"
        );
        assert_eq!(shared.snapshot().version, 0, "still nothing published");
    }

    /// The bug this whole change exists for: the retry never converged, so the
    /// subagent spent its entire tool-call budget on one call that could not
    /// succeed. Past the budget OutRig reports on its behalf, which both ends
    /// the loop and unblocks a parent waiting in `get_result`.
    #[tokio::test]
    async fn a_third_truncated_attempt_publishes_the_failure_and_stops_asking() {
        let (tool, shared) = set_result_tool();

        truncated_call(&tool).await.expect_err("first attempt");
        truncated_call(&tool).await.expect_err("second attempt");
        let err = truncated_call(&tool).await.expect_err("past the budget");

        assert!(
            err.to_string().contains("Stop calling outrig__set_result"),
            "got: {err}"
        );
        assert_eq!(
            shared.snapshot().outcome,
            Some(Outcome::Error(TRUNCATED_REPORT.to_string())),
            "the parent must be told the cause, not left waiting"
        );
        assert_eq!(
            shared.snapshot().state,
            crate::subagent::state::RunState::Running,
            "publishing does not end the round"
        );
    }

    /// The give-up used to reset its own budget, because it published and
    /// `publish` cleared the tally -- so the fourth call counted as a first
    /// attempt and the three-strike cycle restarted, re-warning and
    /// re-publishing for the rest of the round. Stopping at the third call is
    /// exactly what hid it, so this test keeps going.
    #[tokio::test]
    async fn giving_up_stays_given_up() {
        let (tool, shared) = set_result_tool();

        for _ in 0..3 {
            truncated_call(&tool).await.expect_err("up to the give-up");
        }
        let version_at_give_up = shared.snapshot().version;

        for call in 4..=9 {
            let err = truncated_call(&tool)
                .await
                .expect_err("every later call is refused the same way");
            assert!(
                err.to_string().contains("already been told"),
                "call {call} restarted the retry cycle: {err}"
            );
        }
        assert_eq!(
            shared.snapshot().version,
            version_at_give_up,
            "the failure is published once, not once per cycle"
        );
    }

    /// Returning `Ok` from the give-up cleared [`crate::llm::RepeatTracker`]'s
    /// count, disarming the one backstop that could still end the round. The
    /// two mechanisms cancelled each other on the exact case both were written
    /// for, so the give-up must read as a failure.
    #[tokio::test]
    async fn every_truncated_call_reads_as_a_failure_to_the_breaker() {
        let (tool, _shared) = set_result_tool();

        for call in 1..=6 {
            assert!(
                truncated_call(&tool).await.is_err(),
                "call {call} succeeded, which would reset the repeat breaker"
            );
        }
    }

    /// The budget counts *consecutive* failures. A subagent that reports, then
    /// truncates while revising, has shown it can produce a body that fits, so
    /// it gets the full budget again rather than being cut off at once.
    #[tokio::test]
    async fn a_successful_report_restores_the_retry_budget() {
        let (tool, shared) = set_result_tool();

        truncated_call(&tool).await.expect_err("first attempt");
        truncated_call(&tool).await.expect_err("second attempt");
        call(&tool, r#"{"status":"result","body":"the short version"}"#)
            .await
            .expect("a body that fits");

        let err = truncated_call(&tool)
            .await
            .expect_err("the budget starts over, so this is asked to retry");
        assert!(err.to_string().contains("shorter"), "got: {err}");
        assert_eq!(
            shared.snapshot().outcome,
            Some(Outcome::Result("the short version".to_string())),
            "the report that fit must survive the later truncation"
        );
    }

    /// #350: the restored budget runs out too. Giving up then published the
    /// cut-off over the report that fit, so the parent was told nothing was
    /// recorded when something was. The report stands, and the subagent is
    /// told so rather than that its parent was told of the cut-off.
    #[tokio::test]
    async fn giving_up_on_a_revision_keeps_the_report_that_fit() {
        let (tool, shared) = set_result_tool();

        call(&tool, r#"{"status":"result","body":"the short version"}"#)
            .await
            .expect("a body that fits");
        truncated_call(&tool).await.expect_err("first attempt");
        truncated_call(&tool).await.expect_err("second attempt");
        for nth in 3..=4 {
            let err = truncated_call(&tool)
                .await
                .expect_err("past the budget, every call is refused");
            assert!(
                err.to_string()
                    .contains("keeps the report you recorded earlier"),
                "call {nth}: {err}"
            );
        }

        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.outcome,
            Some(Outcome::Result("the short version".to_string()))
        );
        assert_eq!(snapshot.version, 1, "nothing is published over it");
    }

    /// #521: the earlier rungs of the ladder offered `status: "error"` too, so
    /// a compliant model replaced the report that fit with it. While the
    /// report stands, no ask mentions an error.
    #[tokio::test]
    async fn a_revision_cut_off_after_a_report_is_not_asked_for_an_error() {
        let (tool, shared) = set_result_tool();

        call(&tool, r#"{"status":"result","body":"the findings"}"#)
            .await
            .expect("a body that fits");
        for nth in 1..=2 {
            let err = truncated_call(&tool).await.expect_err("cut off");
            let text = err.to_string();
            assert!(
                text.contains("recorded earlier stands"),
                "call {nth}: {text}"
            );
            assert!(!text.contains("\"error\""), "call {nth}: {text}");
        }

        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.outcome,
            Some(Outcome::Result("the findings".to_string()))
        );
        assert_eq!(snapshot.version, 1);
    }

    /// A report the parent has steered past answers nothing, so the short
    /// error is still the right ask.
    #[tokio::test]
    async fn a_revision_cut_off_after_a_steer_is_still_asked_for_a_short_error() {
        let (tool, shared) = set_result_tool();

        call(&tool, r#"{"status":"result","body":"the findings"}"#)
            .await
            .expect("a body that fits");
        shared.accept("and what about the tests?".into());
        let first = truncated_call(&tool).await.expect_err("first attempt");
        assert!(first.to_string().contains("`status: \"error\"`"), "{first}");
        let second = truncated_call(&tool).await.expect_err("second attempt");
        assert!(
            second.to_string().contains("Call once more with"),
            "{second}"
        );
    }

    /// A steer that lands while the model request is in flight is accepted
    /// before the report that request publishes, but that report was written
    /// without it. Until a report follows the steer's delivery, the short
    /// error stays the ask.
    #[tokio::test]
    async fn a_report_written_without_a_steer_does_not_stand_against_it() {
        let (tool, shared) = set_result_tool();

        shared.accept("and what about the tests?".into());
        call(
            &tool,
            r#"{"status":"result","body":"composed before the steer"}"#,
        )
        .await
        .expect("a body that fits");
        let (steers, _) = shared.deliver_injections();
        assert_eq!(steers.len(), 1);
        let first = truncated_call(&tool).await.expect_err("first attempt");
        assert!(first.to_string().contains("`status: \"error\"`"), "{first}");
        let second = truncated_call(&tool).await.expect_err("second attempt");
        assert!(
            second.to_string().contains("Call once more with"),
            "{second}"
        );
    }

    /// `required` keeps `body` present but not meaningful; an empty report
    /// would reach the parent looking like success.
    #[tokio::test]
    async fn set_result_rejects_an_empty_body_without_publishing() {
        let (tool, shared) = set_result_tool();

        let err = call(&tool, r#"{"status":"result","body":"   "}"#)
            .await
            .expect_err("empty body");
        assert!(
            err.to_string().contains("nothing was recorded"),
            "got: {err}"
        );
        assert_eq!(
            shared.snapshot().version,
            0,
            "a rejected call must not publish"
        );
    }
}
