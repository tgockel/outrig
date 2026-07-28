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

use std::sync::Arc;

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::rig_tool::truncate_for_llm;
use crate::subagent::SubagentRegistry;
use crate::subagent::state::{Outcome, SubagentShared, TRUNCATED_REPORT};

/// Wraps a failure as the model-visible tool error.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct BuiltinError(String);

fn fail<T>(message: impl Into<String>) -> Result<T, ToolError> {
    Err(ToolError::ToolCallError(Box::new(BuiltinError(
        message.into(),
    ))))
}

/// Every tool schema below advertises `additionalProperties: false`, and each
/// arg struct carries `deny_unknown_fields` to match. Letting the decoder be
/// laxer than the advertised contract is the same class of drift that made
/// `set_result` accept `{}` while its prose said otherwise.
fn parse_args<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T, ToolError> {
    let value: Value = if args.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args)?
    };
    serde_json::from_value(value).map_err(ToolError::JsonError)
}

fn name_of(tool: &str) -> String {
    outrig::sanitize_tool_name(outrig::RESERVED_SERVER, tool)
}

// ---------------------------------------------------------------- launch ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchArgs {
    name: String,
    #[serde(default)]
    preamble: Option<String>,
    prompt: String,
}

/// `outrig__subagent`: start a subagent and return immediately.
#[derive(Clone)]
pub struct SubagentTool {
    registry: Arc<SubagentRegistry>,
}

impl SubagentTool {
    pub fn new(registry: Arc<SubagentRegistry>) -> Self {
        Self { registry }
    }
}

impl ToolDyn for SubagentTool {
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
        json!({
            "type": "object",
            "properties": {
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
            },
            "required": ["name", "prompt"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: LaunchArgs = parse_args(&args)?;
            match self
                .registry
                .launch(&args.name, args.preamble, args.prompt)
                .await
            {
                Ok(()) => {
                    eprintln!("[outrig] subagent {} started", args.name);
                    Ok(format!(
                        "subagent {:?} started; collect it with outrig__get_result",
                        args.name
                    ))
                }
                Err(e) => fail(e),
            }
        })
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

impl ToolDyn for SubagentSendTool {
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

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: SendArgs = parse_args(&args)?;
            match self.registry.send(&args.name, args.prompt) {
                Ok(what) => Ok(format!("delivered to {:?}: {what}", args.name)),
                Err(e) => fail(e),
            }
        })
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

impl ToolDyn for WaitResultsTool {
    fn name(&self) -> String {
        name_of("wait_results")
    }

    fn description(&self) -> String {
        "Wait until at least min_count of the named subagents have something to \
         collect, then return which ones. Returns names only, not their \
         findings -- call outrig__get_result per subagent to read those, so one \
         call cannot flood your context. Use min_count to react to whichever \
         finishes first. Drop names you have already collected, or this returns \
         them again immediately."
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
                                    Defaults to all of them."
                }
            },
            "required": ["names"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: WaitArgs = parse_args(&args)?;
            let min_count = args.min_count.unwrap_or(args.names.len()).max(1);
            if min_count > args.names.len() {
                return fail(format!(
                    "min_count {min_count} exceeds the {} name(s) given",
                    args.names.len()
                ));
            }
            match self.registry.wait_results(&args.names, min_count).await {
                Ok(ready) => Ok(json!({ "ready": ready }).to_string()),
                Err(e) => fail(e),
            }
        })
    }
}

// ------------------------------------------------------------------- get ---

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetArgs {
    name: String,
}

/// `outrig__get_result`: block until this subagent has something new, return
/// it, and advance the parent's read position past it.
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
}

impl ToolDyn for GetResultTool {
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

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: GetArgs = parse_args(&args)?;
            match self.registry.get_result(&args.name).await {
                Ok(Outcome::Result(text)) => Ok(truncate_for_llm(&text, self.result_cap_bytes)),
                Ok(Outcome::Error(text)) => fail(truncate_for_llm(&text, self.result_cap_bytes)),
                Err(e) => fail(e),
            }
        })
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

impl ToolDyn for SubagentReleaseTool {
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

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args: ReleaseArgs = parse_args(&args)?;
            match self.registry.release(&args.names) {
                Ok(released) => Ok(format!("released: {}", released.join(", "))),
                Err(e) => fail(e),
            }
        })
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
fn looks_truncated(args: &str) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(args) else {
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

/// What every truncated call gets once the round has given up.
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

/// The last ask. Retrying the report is off the table by now, so this asks for
/// the one thing that still fits: a short error the parent can act on.
const SEND_A_SHORT_ERROR: &str = "\
    still nothing recorded: `body` was missing again, so the report is being \
    cut off before it is written. Do not attempt the full report a third time \
    -- it will be cut off in the same place. Call once more with `status: \
    \"error\"` and a one-sentence `body` naming what you found and that the \
    report did not fit.";

/// `outrig__set_result`: the subagent-side tool. Publishes into the inbox the
/// parent reads, and does **not** end the round -- the subagent may keep
/// working and publish again, in which case the later value wins.
#[derive(Clone)]
pub struct SetResultTool {
    shared: Arc<SubagentShared>,
    /// This subagent's handle, for the trace prefix, matching the shape the
    /// agent loop's own traces use.
    label: Arc<str>,
    /// The agent whose turns hit the ceiling, and the ceiling in effect for
    /// it -- which may have come from the agent or from its model. Carried
    /// only to name the fix in the one operator-facing warning below.
    agent_name: Arc<str>,
    max_tokens: Option<u32>,
}

impl SetResultTool {
    pub fn new(
        shared: Arc<SubagentShared>,
        label: &str,
        agent_name: &str,
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            shared,
            label: Arc::from(label),
            agent_name: Arc::from(agent_name),
            max_tokens,
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
        let remedy = match self.max_tokens {
            Some(limit) => format!(
                "the max-tokens in effect for agent {} is {limit} -- raise it",
                self.agent_name
            ),
            // Nothing set it anywhere, so there is no ambiguity about which
            // table to name.
            None => format!(
                "[agents.{}].max-tokens is unset, so the provider's default \
                 applies -- set it explicitly",
                self.agent_name
            ),
        };
        eprintln!(
            "[outrig]   [{}] set_result was cut off before `body`: the model's \
             output-token ceiling is too low for this report. {remedy}.",
            self.label
        );
    }
}

impl ToolDyn for SetResultTool {
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

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            if looks_truncated(&args) {
                // Remember it on the subagent too: if the round goes on to end
                // without publishing, the parent gets the cause rather than a
                // bare "it stopped".
                let attempt = self.shared.note_truncated_attempt();
                // Checked before the attempt count, and latched rather than
                // derived from it, so that everything below happens exactly
                // once per round however many truncated calls keep arriving.
                if self.shared.has_given_up() {
                    return fail(ALREADY_REPORTED);
                }
                return match attempt {
                    1 => {
                        self.warn_about_the_ceiling();
                        fail(SHORTEN_THE_REPORT)
                    }
                    n if n <= TRUNCATION_RETRY_BUDGET => fail(SEND_A_SHORT_ERROR),
                    // Out of retries. Publishing on the subagent's behalf is
                    // what makes this converge: `end_round` sees a round that
                    // published, and a parent blocked in `get_result` wakes now
                    // with the cause instead of once the tool-call budget has
                    // drained into the same failing call.
                    _ => {
                        if self.shared.give_up() {
                            self.shared
                                .publish(Outcome::Error(TRUNCATED_REPORT.to_string()));
                        }
                        fail(ALREADY_REPORTED)
                    }
                };
            }
            let args: SetResultArgs = parse_args(&args)?;
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
                    Ok("result recorded".to_string())
                }
                ResultStatus::Error => {
                    self.shared.publish(Outcome::Error(args.body));
                    Ok("error recorded".to_string())
                }
            }
        })
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
            parse_args(r#"{"name":"audit","prompt":"check the config"}"#).expect("parses");
        assert_eq!(args.name, "audit");
        assert!(args.preamble.is_none());
    }

    #[test]
    fn wait_args_accept_an_omitted_min_count() {
        let args: WaitArgs = parse_args(r#"{"names":["a","b"]}"#).expect("parses");
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
            SetResultTool::new(shared.clone(), "audit", "primary", None),
            shared,
        )
    }

    /// The truncated shape, as it arrives from the provider: `status` present
    /// because it generates first, `body` lost to the ceiling.
    async fn truncated_call(tool: &SetResultTool) -> Result<String, ToolError> {
        tool.call(r#"{"status":"result"}"#.to_string()).await
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
        assert!(parse_args::<SetResultArgs>("{}").is_err());
        assert!(parse_args::<SetResultArgs>(r#"{"status":"result"}"#).is_err());
        assert!(parse_args::<SetResultArgs>(r#"{"body":"a"}"#).is_err());
        assert!(parse_args::<SetResultArgs>(r#"{"status":"maybe","body":"a"}"#).is_err());
    }

    #[tokio::test]
    async fn set_result_publishes_under_the_status_it_was_given() {
        let (tool, shared) = set_result_tool();

        tool.call(r#"{"status":"result","body":"found it"}"#.to_string())
            .await
            .expect("a result publishes");
        assert_eq!(
            shared.snapshot().outcome,
            Some(Outcome::Result("found it".to_string()))
        );

        tool.call(r#"{"status":"error","body":"could not finish"}"#.to_string())
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
        tool.call(r#"{"status":"result","body":"the short version"}"#.to_string())
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

    /// `required` keeps `body` present but not meaningful; an empty report
    /// would reach the parent looking like success.
    #[tokio::test]
    async fn set_result_rejects_an_empty_body_without_publishing() {
        let (tool, shared) = set_result_tool();

        let err = tool
            .call(r#"{"status":"result","body":"   "}"#.to_string())
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
