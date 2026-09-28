//! `submit_python`: the model's one tool, which runs source in the session's
//! interpreter and hands back what happened.
//!
//! Every outcome the host can report comes back as text the model reads,
//! including the ones that are not successes. Code that raises is a result, not
//! a tool failure -- the traceback is what the model needs -- and an outcome the
//! host cannot vouch for says so in words that forbid running it again, since
//! nothing is ever rolled back. Only a submission that could not be made at all
//! is a tool error.
//!
//! The wait is `recovery`'s, so a call that the user interrupts, or whose
//! code the host finds spinning, still ends with the execution's own outcome
//! -- and says what was done to it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use rig::tool::{ToolDyn, ToolError};
use rig::wasm_compat::WasmBoxedFuture;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::python::host::{Interpreter, Late, Outcome, Report, Unknown};
use crate::python::recovery::{self, ATTEMPTS, GaveUp, Press, Presses, Timings, Verdict, Waited};

/// What the model calls the tool.
pub(crate) const NAME: &str = "submit_python";

/// What a run that showed nothing reads as, so a successful call is never an
/// empty string the model has to guess the meaning of.
const NO_OUTPUT: &str = "(no output)";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    source: String,
}

/// The schema advertises `additionalProperties: false` and the struct carries
/// `deny_unknown_fields` to match, so the decoder is never laxer than the
/// published contract. Empty arguments decode as `{}`, which then fails for
/// the missing `source` rather than as malformed JSON.
fn parse_args(args: &str) -> Result<Args, ToolError> {
    let args = if args.trim().is_empty() { "{}" } else { args };
    serde_json::from_str(args).map_err(ToolError::JsonError)
}

/// Told the source of each submission sent to the interpreter to run.
pub(crate) type SubmitObserver = Box<dyn Fn(&str) + Send + Sync>;

/// Where an observer is put for the tool to find. The tool is handed to rig
/// when the agent is built, before any observer exists, so the two share this
/// rather than the observer being passed in.
pub(crate) type ObserverSlot = Arc<Mutex<Option<SubmitObserver>>>;

/// Where the user's interrupt goes: the execution a call is waiting on, and
/// the turn that call belongs to. Shared by the tool, the round's hook, and
/// whoever relays the interrupts, which does so while the round runs.
#[derive(Clone, Default)]
pub(crate) struct Interrupts {
    presses: Presses,
    /// Set once the user has stopped a call. The turn's later calls are then
    /// not run, so nothing the model wrote after the stopped call runs before
    /// it has read how that call ended.
    turn_stopped: Arc<AtomicBool>,
}

impl Interrupts {
    /// Relay an interrupt to the execution a call is waiting on, returning
    /// what that does as a sentence for the user; `None` if no call is
    /// waiting.
    pub(crate) fn press(&self) -> Option<String> {
        Some(match self.presses.press()? {
            Press::Stop(id) => format!(
                "stopping execution {id} -- the model will see how it ended (interrupt again to \
                 stop waiting for it)"
            ),
            Press::GiveUp(id) => format!(
                "no longer waiting for execution {id}: it keeps the interpreter until it finishes"
            ),
        })
    }

    /// Whether the user has stopped a call in the current turn.
    pub(crate) fn turn_stopped(&self) -> bool {
        self.turn_stopped.load(Ordering::SeqCst)
    }

    /// A new turn starts with nothing stopped.
    pub(crate) fn clear_turn(&self) {
        self.turn_stopped.store(false, Ordering::SeqCst);
    }
}

/// The tool, holding the interpreter it submits to and the byte ceiling on
/// what it hands back.
pub(crate) struct SubmitPython {
    interpreter: Interpreter,
    result_max_bytes: usize,
    /// Late results taken from the interpreter that the last result had no
    /// room to report. `take_late` hands each out once, so they are held here
    /// and lead the next result rather than being lost.
    unreported: Mutex<Vec<Late>>,
    on_submit: ObserverSlot,
    interrupts: Interrupts,
}

impl SubmitPython {
    pub(crate) fn new(interpreter: Interpreter, result_max_bytes: usize) -> Self {
        Self {
            interpreter,
            result_max_bytes,
            unreported: Mutex::new(Vec::new()),
            on_submit: ObserverSlot::default(),
            interrupts: Interrupts::default(),
        }
    }

    /// The slot this tool reads its observer from.
    pub(crate) fn observer_slot(&self) -> ObserverSlot {
        Arc::clone(&self.on_submit)
    }

    /// Where presses reach this tool's calls.
    pub(crate) fn interrupts(&self) -> Interrupts {
        self.interrupts.clone()
    }

    /// As if an earlier result had had no room for `late`.
    #[cfg(test)]
    pub(crate) fn with_unreported(self, late: Vec<Late>) -> Self {
        *self
            .unreported
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = late;
        self
    }
}

impl ToolDyn for SubmitPython {
    fn name(&self) -> String {
        NAME.to_string()
    }

    fn description(&self) -> String {
        "Run Python in this session's persistent interpreter, inside the project's container. \
         Top-level `await` is supported. Names you bind stay bound for later calls, and a \
         trailing expression echoes its repr. Code that raises comes back as its traceback \
         rather than as a tool failure, so read it and carry on. Nothing is rolled back: \
         whatever ran before an error happened. The ordinary standard library is here -- \
         pathlib, open(), subprocess, asyncio, json -- operating on the container. Output is \
         bounded, so print summaries rather than raw data. Code that keeps the event loop from \
         turning while it keeps a CPU busy is taken for a runaway after about half a minute \
         and interrupted, so run long computations with `await asyncio.to_thread(...)`; \
         waiting on a subprocess or a sleep is not."
            .to_string()
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "Python source to run. May be several lines."
                }
            },
            "required": ["source"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: String) -> WasmBoxedFuture<'a, Result<String, ToolError>> {
        Box::pin(async move {
            let Args { source } = parse_args(&args)?;
            // Nothing was sent, so this one is a failure of the tool rather than
            // an outcome of the code.
            let execution = self
                .interpreter
                .submit(&source)
                .map_err(|e| ToolError::ToolCallError(e.into()))?;
            tracing::debug!(execution = %execution.id(), "submitted");
            // Only source that went to run: a refusal is the model's to read,
            // not something to show as running. Interrupts reach it from
            // before it is shown, since that is what a person interrupts.
            let queued = execution.queued();
            let _waiting = queued.then(|| self.interrupts.presses.waiting_on(execution.id()));
            if queued
                && let Some(observer) = &*self
                    .on_submit
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
            {
                observer(&source);
            }
            let settled = recovery::settle(
                &self.interpreter,
                execution,
                &self.interrupts.presses,
                &Timings::default(),
            )
            .await;
            // From what was done rather than from the press, which may have
            // lost the race to the outcome and done nothing.
            if settled.waited.user_stopped {
                self.interrupts.turn_stopped.store(true, Ordering::SeqCst);
            }
            // Taken after the outcome, so anything that arrived while this ran
            // is reported now rather than a call later -- after whatever an
            // earlier result had no room for, which is older.
            let mut unreported = self
                .unreported
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let mut late = std::mem::take(&mut *unreported);
            late.extend(self.interpreter.take_late());
            let (text, rest) = render(
                late,
                &settled.outcome,
                settled.waited,
                self.result_max_bytes,
            );
            *unreported = rest;
            Ok(text)
        })
    }
}

/// The longest traceback line a status repeats.
const EXCEPTION_LINE_MAX: usize = 300;

/// Room kept in the status for the line saying how many late results wait for
/// a later call.
const UNREPORTED_NOTICE_MAX: usize = 96;

/// What the model reads for `outcome`, with what the host did to it while it
/// waited, and for any results that arrived with nobody waiting for them, in
/// at most `max` bytes.
///
/// It comes in two parts. The status says how each execution ended -- raised,
/// refused, unknown and not to be re-run -- and is kept whole. The detail is
/// what they printed and the traceback, and it is what gets cut to fit. Cutting
/// the tail of one string instead would take a trailing traceback with it,
/// and with it the only sign that the code raised.
///
/// A late result goes in the next tool result because that is where the
/// interpreter's own between-execution output already travels, and a model
/// that was told an execution's outcome was unknown is owed the answer the
/// first time there is somewhere to put it. Late statuses are added whole, in
/// order, while they fit; those that do not are counted in the status and
/// handed back, for the caller to report next time. At the smallest ceiling
/// config allows there is room for the current status and at least one.
pub(crate) fn render(
    mut late: Vec<Late>,
    outcome: &Outcome,
    waited: Waited,
    max: usize,
) -> (String, Vec<Late>) {
    let current = render_outcome(outcome, waited);
    let mut status = String::new();
    if let Some(line) = &current.status {
        status.push_str(line);
        status.push('\n');
    }
    let mut shown = 0;
    for record in &late {
        let line = format!(
            "[execution {}, whose call stopped waiting for it, has since finished: {}]\n",
            record.id,
            summary(&record.outcome),
        );
        if status.len() + line.len() + UNREPORTED_NOTICE_MAX > max {
            break;
        }
        status.push_str(&line);
        shown += 1;
    }
    let unreported = late.split_off(shown);
    if !unreported.is_empty() {
        let _ = writeln!(
            status,
            "[{} more results that arrived late will be reported with a later call]",
            unreported.len()
        );
    }

    let mut detail = String::new();
    if !late.is_empty() && !current.detail.is_empty() {
        detail.push_str("[this call]\n");
    }
    detail.push_str(&current.detail);
    for record in &late {
        let rendered = render_outcome(&record.outcome, Waited::default()).detail;
        if !rendered.is_empty() {
            push_block(
                &mut detail,
                &format!("[execution {}]\n{rendered}", record.id),
            );
        }
    }

    // Only a ceiling far below config's floor leaves no room past the current
    // status; cut it rather than exceed the bound.
    let text = if status.len() >= max {
        truncate_for_llm(&status, max)
    } else {
        status.push_str(&truncate_for_llm(&detail, max - status.len()));
        status
    };
    (text, unreported)
}

/// One outcome, split into what must survive any bound and what may be cut.
struct Rendered {
    /// How it ended, when that is not simply "it ran": one short paragraph.
    status: Option<String>,
    detail: String,
}

/// How a late result ended, as one phrase.
fn summary(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Ok(_) => "it ran to completion".to_string(),
        Outcome::Error { traceback, .. } => format!("it raised {}", exception_line(traceback)),
        Outcome::Refused { .. } => "it was not run".to_string(),
        Outcome::Unknown(_) => "its outcome is unknown".to_string(),
    }
}

/// What a runaway's status adds, so the model does not write another.
const RUNAWAY_ADVICE: &str = "Run long computations with `await asyncio.to_thread(...)`, which \
     keeps the loop turning.";

/// What stopping Python leaves running.
const ORPHANS: &str = "Stopping Python does not stop the processes or threads it started, which \
     may still be running.";

/// What the host did while it waited, then how the code ended -- `raised`, or
/// run to completion -- as one status; `None` when the host did nothing.
///
/// A runaway interrupt is described without naming its target. It may have
/// ended this code, a task another execution left running, or neither, since
/// code can catch it; the outcome and the output say which.
fn stopped(waited: Waited, raised: Option<&str>) -> Option<String> {
    const RUNAWAY: &str = "the event loop stopped answering while a CPU stayed busy, and OutRig \
         interrupted the code spinning on it";
    Some(
        match (waited.user_stopped, waited.runaway_interrupted, raised) {
            (false, false, _) => return None,
            (true, false, Some(raised)) => {
                format!(
                    "[the user interrupted this call, and this code raised {raised}. {ORPHANS}]"
                )
            }
            (true, false, None) => {
                "[the user interrupted this call, but it ran to completion]".into()
            }
            (false, true, Some(raised)) => {
                format!("[this code raised {raised}: {RUNAWAY}. {RUNAWAY_ADVICE}]")
            }
            (false, true, None) => {
                format!(
                    "[during this call {RUNAWAY}; the call then ran to completion. {RUNAWAY_ADVICE}]"
                )
            }
            (true, true, ended) => format!(
                "[the user interrupted this call, and {RUNAWAY}; {}. {ORPHANS} {RUNAWAY_ADVICE}]",
                match ended {
                    Some(raised) => format!("this code raised {raised}"),
                    None => "the call then ran to completion".to_string(),
                }
            ),
        },
    )
}

fn render_outcome(outcome: &Outcome, waited: Waited) -> Rendered {
    match outcome {
        Outcome::Ok(report) => {
            let text = render_report(report);
            Rendered {
                status: stopped(waited, None),
                detail: if text.trim().is_empty() {
                    NO_OUTPUT.to_string()
                } else {
                    text
                },
            }
        }
        Outcome::Error { report, traceback } => {
            let mut detail = render_report(report);
            push_block(&mut detail, traceback);
            let raised = exception_line(traceback);
            Rendered {
                status: Some(
                    stopped(waited, Some(raised))
                        .unwrap_or_else(|| format!("[this code raised {raised}]")),
                ),
                detail,
            }
        }
        Outcome::Refused { holder } if let Some(verdict) = waited.holder => Rendered {
            status: Some(format!(
                "Not run: execution {holder} still holds the interpreter, which runs one \
                 execution at a time, and nothing is waiting for it any more. {} Nothing from \
                 this call ran.",
                match verdict {
                    Verdict::Spinning => format!(
                        "OutRig found it spinning and has interrupted it; if that ends it, its \
                         result will be reported with a later call. {RUNAWAY_ADVICE}"
                    ),
                    Verdict::Blocked => "It is blocked in a call such as a subprocess wait or \
                                         a sleep, and frees the interpreter when that returns."
                        .into(),
                    Verdict::Turning => "It is suspended on an await that has not resolved.".into(),
                    Verdict::Starved => "The interpreter is not answering at all: native code \
                                         is holding it, and nothing can interrupt that."
                        .into(),
                }
            )),
            detail: String::new(),
        },
        Outcome::Refused { holder } => Rendered {
            status: Some(format!(
                "Not run: execution {holder} has not finished, and the interpreter runs one \
                 execution at a time. Nothing from this call ran. Its result will be reported \
                 with a later call."
            )),
            detail: String::new(),
        },
        Outcome::Unknown(Unknown::Exited { id, cause }) => Rendered {
            status: Some(format!(
                "The outcome of execution {id} is unknown: the Python interpreter exited before \
                 it reported. Some or all of what it does may have happened. Check for its \
                 effects rather than running it again. The interpreter is gone, so later calls \
                 will fail."
            )),
            detail: format!("How it exited: {cause}"),
        },
        Outcome::Unknown(Unknown::Unresolved { id }) => Rendered {
            status: Some(format!(
                "The outcome of execution {id} is unknown: {}. It may still be running, and \
                 holds the interpreter until it finishes, so later calls are refused until \
                 then. Do not run it again; its result will be reported with a later call.",
                match waited.gave_up {
                    Some(GaveUp::User) => {
                        "the user interrupted it twice, and the host stopped waiting for it".into()
                    }
                    Some(GaveUp::Runaway) => format!(
                        "its event loop kept spinning through {ATTEMPTS} interrupts, and the \
                         host stopped waiting for it"
                    ),
                    None => "the host stopped waiting for it".to_string(),
                }
            )),
            detail: String::new(),
        },
    }
}

/// The traceback's last line -- the exception and its message -- bounded.
fn exception_line(traceback: &str) -> &str {
    let line = traceback
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("an exception")
        .trim();
    &line[..floor_char_boundary(line, line.len().min(EXCEPTION_LINE_MAX))]
}

/// What an execution wrote, then what it lost past the bound, then what
/// earlier executions wrote since, each labeled with the execution to blame.
fn render_report(report: &Report) -> String {
    let mut text = report.output.clone();
    if report.dropped > 0 {
        push_block(
            &mut text,
            &format!("[{} more bytes of output were dropped]", report.dropped),
        );
    }
    for background in &report.background {
        let mut section = format!(
            "[output from execution {}, written after it reported]\n{}",
            background.id, background.output
        );
        if background.dropped > 0 {
            push_block(
                &mut section,
                &format!("[{} more bytes were dropped]", background.dropped),
            );
        }
        push_block(&mut text, &section);
    }
    text
}

/// Append `block` on a line of its own.
fn push_block(text: &mut String, block: &str) {
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(block);
}

/// Bound `result` to `max` bytes, ending a cut one with a marker saying what
/// was cut and what to do instead.
///
/// Copied from `outrig-cli`'s `rig_tool.rs`, whose marker advises narrowing a
/// query; this one's advises printing less.
pub(crate) fn truncate_for_llm(result: &str, max: usize) -> String {
    if result.is_empty() || result.len() <= max {
        return result.to_string();
    }
    if max == 0 {
        return String::new();
    }

    let original_len = result.len();
    let mut cut = max.saturating_sub(truncation_marker(original_len, max, 0).len());
    loop {
        cut = floor_char_boundary(result, cut.min(result.len()));
        let marker = truncation_marker(original_len, max, cut);
        if marker.len() >= max {
            return truncate_marker(&marker, max);
        }

        let content_budget = max - marker.len();
        if cut <= content_budget {
            let mut truncated = String::with_capacity(cut + marker.len());
            truncated.push_str(&result[..cut]);
            truncated.push_str(&marker);
            debug_assert!(truncated.len() <= max);
            return truncated;
        }

        cut = content_budget;
    }
}

fn truncation_marker(original_len: usize, max: usize, kept: usize) -> String {
    let dropped = original_len.saturating_sub(kept);
    format!(
        concat!(
            "\n\n[outrig: tool result truncated]\n",
            "  original size: {original_len} bytes\n",
            "  max:           {max} bytes\n",
            "  kept:          first {kept} bytes; trailing {dropped} bytes dropped.\n\n",
            "  This result was larger than the configured max. Print less: slice or\n",
            "  summarize the value, or keep it bound to a name and look at the part\n",
            "  you need. Printing the same thing again will be truncated the same way.",
        ),
        original_len = original_len,
        max = max,
        kept = kept,
        dropped = dropped,
    )
}

fn truncate_marker(marker: &str, max: usize) -> String {
    let cut = floor_char_boundary(marker, max.min(marker.len()));
    marker[..cut].to_string()
}

fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}
