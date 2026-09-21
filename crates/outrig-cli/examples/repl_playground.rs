//! Drive the real REPL loop -- and so the real rustyline editor -- with no
//! container, no model, and no config.
//!
//! `Repl::run` is what `outrig run` calls, and it asks `line_source::auto()`
//! for a source, so on a terminal this gets the same `EditorSource` a real
//! session does. What is stubbed out is only the callback: prompts come back
//! echoed instead of going to an LLM.
//!
//! ```sh
//! cargo run -p outrig-cli --features internal-test-api --example repl_playground
//! ```
//!
//! The feature is required: `repl` and `error` are `pub(crate)` without it. A
//! plain `cargo run --example` appears to work only because this crate's
//! dev-dependency on itself turns the feature on for local builds; the example
//! is excluded from the published package for the same reason `tests/` is.
//!
//! Worth trying, in rough order of how likely each is to be broken:
//!
//! - Up/Down after a few prompts -- history recall, in memory, this session only.
//! - Ctrl-A / Ctrl-E / Ctrl-W / Alt-B / Alt-F -- readline editing.
//! - Ctrl-R -- reverse search through this session's prompts.
//! - Ctrl-C at the prompt once (returns to prompt), then twice in a row (exits).
//! - Ctrl-D on an empty line -- exits.
//! - `/help`, `/quit`, and `/bogus` -- dispatch, which must not vary with the source.
//! - `slow` -- a 30s turn, so SIGINT mid-callback is reachable. That path is a
//!   real signal, not a keystroke: rustyline is not holding the terminal then.
//! - A multi-line paste -- every line after the first is what `buffer-redux`
//!   exists to keep.
//! - Redirect stdout (`... --example repl_playground > out.txt`) and confirm
//!   out.txt gets only the echoed replies: no prompt, no banner, no echo of
//!   what you typed.
//! - `TERM=dumb cargo run ...` -- declines the editor and falls back to plain
//!   line reads, which is the check that keeps the prompt out of that file.

use std::time::Duration;

use outrig_cli::error::Result;
use outrig_cli::repl::{HelpEntry, Repl};

const COMMANDS: &[HelpEntry] = &[
    HelpEntry {
        syntax: "/echo <text>",
        description: "print the args back, to show dispatch",
    },
    HelpEntry {
        syntax: "/slow",
        description: "a command that takes a moment",
    },
];

const BANNER: &str = "[outrig] repl playground -- no container, no model\n\
                      [outrig] prompts are echoed; /help lists commands";

#[tokio::main]
async fn main() -> Result<()> {
    let on_prompt = |line: String| async move {
        // A turn long enough to interrupt. The REPL cancels this future on
        // SIGINT, so the sleep is what makes that path reachable by hand.
        if line == "slow" {
            eprintln!("[outrig] thinking for 30s -- press Ctrl-C to cancel the turn");
            tokio::time::sleep(Duration::from_secs(30)).await;
            return Result::Ok("...finished without being interrupted".to_string());
        }
        Result::Ok(format!("echo: {line}"))
    };

    let on_command = |name: String, args: Vec<String>| async move {
        match name.as_str() {
            "echo" => Some(format!("[outrig] args: {args:?}")),
            "slow" => {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Some("[outrig] done".to_string())
            }
            _ => None,
        }
    };

    Repl::run(BANNER, COMMANDS, on_prompt, on_command).await
}
