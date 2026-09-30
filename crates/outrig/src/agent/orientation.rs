//! What the model is told before anything else: the system prompt's opening,
//! ahead of whatever preamble the agent's config supplies.
//!
//! It carries only what an agent cannot learn by looking and will need every
//! round (`plan/phase/0003-python/discovery.md`). That names persist and that
//! output is bounded while values are not is already in `submit_python`'s
//! description, which the model also reads every round, so it is not repeated
//! here. How the user reaches it is not, and every round opens on a count of
//! messages that only makes sense with it. `runtime.wait` is that page's
//! worked example: its signature is `asyncio.wait`'s on purpose, so nothing in
//! it says that it watches the channels and raises while a message waits.
//!
//! What this interpreter can import is here too, because the failure it heads
//! off -- installing a compiled package, and finding it will not load -- costs
//! a round to learn by trying. The rest is in docstrings: the orientation names
//! `help(runtime)` and `runtime.python`, and those say the details.

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
         file, run a command, or change anything.\n\n\
         The user reaches you through messages on `runtime.channels[\"user\"]`. You are told how \
         many are waiting there, not what they say: `await runtime.channels[\"user\"].receive()` \
         takes the next one, whose `.body` is its text, and `.pending()` counts them. `await \
         runtime.channels[\"user\"].send(text)` sends the user a message. What you write \
         yourself is commentary; send what you mean the user to have. A send is also the only \
         way code still running after you stop writing can reach them.\n\n\
         `runtime.wait` is `asyncio.wait` -- the same arguments, the same `(done, pending)` \
         back, and a timeout that cancels nothing -- except that while a message waits unread, \
         one there before the call included, it raises `runtime.MessageAvailable`, which \
         `except Exception` does not catch. The message stays queued and the tasks keep \
         running. A bare `await` is not ended by a message, and when the user interrupts your \
         code, what it awaits is cancelled; `runtime.wait` leaves those tasks running.\n\n",
    );
    if let Some(workspace) = workspace {
        text.push_str(&format!(
            "Your working directory is {}, which holds the project's files; its Python modules \
             import too.\n\n",
            workspace.display()
        ));
    }
    text.push_str(
        "The interpreter is OutRig's own static CPython, with the standard library. `pip install` \
         adds pure-Python packages, which import at once. Nothing compiled loads here, so numpy and other packages with compiled parts \
         do not import however they are installed; `runtime.python` says what can run here and \
         where such code can run instead. Programs the container's image provides are reachable \
         through `subprocess`. `help(x)` describes anything, and `help(runtime)` describes what \
         OutRig gives you.",
    );
    if let Some(configured) = configured {
        text.push_str("\n\n");
        text.push_str(configured);
    }
    text
}
