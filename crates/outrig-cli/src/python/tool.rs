//! The `python_execute` tool: the agent's whole surface on the world.
//!
//! Hand-written `impl ToolDyn` with a hand-written schema, which is how every tool in this
//! workspace is built.

use std::sync::Arc;

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use super::kernel::PythonKernel;

/// Carries a kernel failure into rig's error channel without leaking our concrete type.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct PythonToolError(String);

/// Returned when code runs but shows nothing, so a successful call is never an empty string the
/// model has to guess the meaning of.
const NO_OUTPUT: &str = "(no output)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    source: String,
}

/// The schema advertises `additionalProperties: false` and the arg struct carries
/// `deny_unknown_fields` to match, so the decoder is never laxer than the published contract.
/// Empty or whitespace-only arguments decode as `{}` rather than failing as malformed JSON.
fn parse_args(args: &str) -> Result<Args, ToolError> {
    let value: Value = if args.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args)?
    };
    serde_json::from_value(value).map_err(ToolError::JsonError)
}

pub struct PythonExecuteTool {
    kernel: Arc<PythonKernel>,
    result_cap_bytes: usize,
}

impl PythonExecuteTool {
    pub fn new(kernel: Arc<PythonKernel>, result_cap_bytes: usize) -> Self {
        Self {
            kernel,
            result_cap_bytes,
        }
    }
}

impl ToolDyn for PythonExecuteTool {
    fn name(&self) -> String {
        "python_execute".to_string()
    }

    fn description(&self) -> String {
        "Run Python in this session's persistent interpreter, inside the project's container. \
         Top-level `await` is supported. Names you bind stay bound for later executions and \
         later turns, and a bare expression on its own line echoes its repr. Code that raises \
         comes back as a traceback rather than as a tool failure, so read it and try again. \
         The global `runtime` reaches the user's channel and the interruptible wait; use \
         help() and dir() on it. The ordinary standard library is here -- pathlib, open(), \
         dataclasses, asyncio, subprocess, json, csv, ssl -- operating on the container's \
         filesystem and network."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Python source to execute. May be several lines."
                }
            },
            "required": ["source"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let args = parse_args(&args)?;
            match self.kernel.execute(args.source).await {
                // A raise is not a tool failure -- the tool worked, the code raised, and the
                // traceback is the thing the model needs. Exceptions are how this architecture
                // reports failure to the model in the first place.
                Ok(outcome) => {
                    let mut text = outcome.output;
                    if let Some(error) = outcome.error {
                        if !text.is_empty() && !text.ends_with('\n') {
                            text.push('\n');
                        }
                        text.push_str(&error);
                    }
                    if text.trim().is_empty() {
                        text = NO_OUTPUT.to_string();
                    }
                    Ok(crate::rig_tool::truncate_for_llm(&text, self.result_cap_bytes))
                }
                // A dead or unreachable interpreter is a real tool failure.
                Err(e) => Err(ToolError::ToolCallError(Box::new(PythonToolError(
                    e.to_string(),
                )))),
            }
        })
    }
}
