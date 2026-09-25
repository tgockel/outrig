//! What the model is told before anything else: the system prompt's opening,
//! ahead of whatever preamble the agent's config supplies.
//!
//! It carries only what an agent cannot learn by looking and will need every
//! round (`plan/phase/0003-python/discovery.md`). That names persist and that
//! output is bounded is already in `submit_python`'s description, which the
//! model also reads every round, so it is not repeated here.

use std::path::Path;

/// The orientation, then `configured` after a blank line when there is one.
///
/// `workspace` is where the interpreter's working directory is: the primary's
/// `-w`, which is its workspace mount. Without one there is nothing true to
/// say about it, so the line is left out.
pub(crate) fn preamble(workspace: Option<&Path>, configured: Option<&str>) -> String {
    let mut text = String::from(
        "You act on this project by writing Python. Your one tool, `submit_python`, runs code in \
         a persistent interpreter inside the project's container; it is the only way to read a \
         file, run a command, or change anything.\n\n",
    );
    if let Some(workspace) = workspace {
        text.push_str(&format!(
            "Your working directory is {}, which holds the project's files.\n\n",
            workspace.display()
        ));
    }
    text.push_str(
        "The interpreter is a static CPython with the standard library and nothing more. `pip \
         install` does not work, and a third-party module with compiled parts cannot be imported \
         here. Programs the container's image provides are reachable through `subprocess`.",
    );
    if let Some(configured) = configured {
        text.push_str("\n\n");
        text.push_str(configured);
    }
    text
}
