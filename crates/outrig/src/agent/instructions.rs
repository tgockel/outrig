//! The repository's own instructions for agents, which the primary agent's
//! system prompt carries after its configured preamble.
//!
//! The file is read inside the session's container, from the workspace's
//! container path, by the interpreter that is already running there. What the
//! prompt carries is therefore what the agent would see by reading the file
//! itself, and a symlink in the workspace can lead only to what the container
//! has. The read is not one of the agent's executions, so nothing of it is in
//! the agent's history.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::RepoInstructions;
use crate::python::host::{FileText, Interpreter};

/// The most of the file the prompt carries, in bytes. Every model call pays for
/// the system prompt, and a window it leaves too little room in cannot be built
/// over.
const MAX_BYTES: usize = 32 * 1024;

/// Where a workspace keeps its instructions, from its root, in the order they
/// are tried: the first that exists is the file, and the others are not read.
const CANDIDATES: [&str; 2] = ["AGENTS.md", ".agents/AGENTS.md"];

/// A repository's instructions for agents, as far as the prompt carries them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instructions {
    /// Where the file is, as the agent names it.
    path: PathBuf,
    /// What was read of it: up to [`MAX_BYTES`] of its text, and its size,
    /// which is more than the text holds when it was cut short.
    file: FileText,
}

/// The instructions `workspace` holds, read through `interpreter`, or `None`
/// when it holds none, or when `mode` leaves them out, or when the session has
/// no workspace.
///
/// A file with nothing in it is no instructions, and the next place is tried;
/// one the cap cut short is not judged by the part that was read. A file that
/// cannot be read -- it is not UTF-8, or its mode forbids it -- is the file all
/// the same: it is said so, and nothing is loaded, rather than whichever file
/// comes after it. Nothing here fails a session's start.
pub(crate) async fn load(
    interpreter: &Interpreter,
    workspace: Option<&Path>,
    mode: RepoInstructions,
) -> Option<Instructions> {
    let (Some(workspace), RepoInstructions::AgentsMd) = (workspace, mode) else {
        return None;
    };
    for name in CANDIDATES {
        let path = workspace.join(name);
        match interpreter.read_file(&path, MAX_BYTES).await {
            Ok(Some(file)) if file.size > file.text.len() || !file.text.trim().is_empty() => {
                return Some(Instructions { path, file });
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "outrig::harness",
                    "the agent's system prompt does not carry {}: {e}",
                    path.display()
                );
                return None;
            }
        }
    }
    None
}

impl Instructions {
    /// What the system prompt adds: where the file is, so the agent can read
    /// it again, then its text, then a note when the file was longer than the
    /// prompt carries.
    pub(crate) fn section(&self) -> String {
        let path = self.path.display();
        let FileText { size, text } = &self.file;
        let mut section = format!(
            "The project's instructions for agents, from `{path}`:\n\n{}",
            text.trim_end()
        );
        let shown = text.len();
        if *size > shown {
            let _ = write!(
                section,
                "\n\n[outrig: `{path}` is {size} bytes; only the first {shown} are shown here. Read \
                 the rest from the file when you need it.]",
            );
        }
        section
    }
}

#[cfg(test)]
#[path = "instructions_tests.rs"]
mod instructions_tests;
