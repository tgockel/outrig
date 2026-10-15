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
//! a round to learn by trying. So is what each model call is sent of the
//! conversation: an agent that cannot see the middle of it would not know to
//! look for it. The rest is in docstrings: the orientation names
//! `help(runtime)` and `runtime.python`, and those say the details.
//!
//! A session's bindings are here too, each by name and description: names an
//! agent cannot learn by looking, and any round may need one. So is the one
//! rule about them that looking cannot teach -- a call on one blocks the
//! agent's event loop until the host answers -- since
//! `repo.head.commit.hexsha` reads like a plain attribute chain
//! (`plan/phase/0003-python/discovery.md`).

use std::fmt::Write as _;
use std::path::Path;

use super::history::Window;
use crate::harness::bindings::Brief;

/// The orientation, then `configured` after a blank line when there is one.
///
/// `workspace` is where the interpreter's working directory is: the primary's
/// `-w`, which is its workspace mount. Without one there is nothing true to
/// say about it, so the line is left out. `bindings` are the session's, in its
/// order; with none, nothing is said of them. `window` is what each model call
/// is sent of the conversation.
pub(crate) fn preamble(
    workspace: Option<&Path>,
    bindings: &[Brief],
    configured: Option<&str>,
    window: Window,
) -> String {
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
    let Window { first, recent } = window;
    text.push_str(&format!(
        "Your whole conversation is in `runtime.history.turns`, a `Turn` for each of your model \
         calls, holding what you wrote and each call's source and result. Reading it from code \
         costs no context: only what you print is seen. Each call is sent less than all of it: \
         the first {first} rounds, the {recent} before this one, this one, and any turn you name \
         with `runtime.context.promote(turn)` until `runtime.context.demote(turn)`. Each call is \
         also held to the model's context window, which can leave out more, never your latest \
         call. A round begins each time you are told messages are waiting, and its opening line \
         says how many earlier turns are left out.\n\n"
    ));
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
    if !bindings.is_empty() {
        text.push_str(&hosted(bindings));
    }
    if let Some(configured) = configured {
        text.push_str("\n\n");
        text.push_str(configured);
    }
    text
}

/// What the agent is told of the session's bindings, of which there is at
/// least one: each by name and description, and how a hosted object is used,
/// with the first one's name standing in for any of them.
fn hosted(bindings: &[Brief]) -> String {
    let example = &bindings[0].name;
    let mut text = String::from(
        "\n\nThese names are objects held by processes on the host, outside this container, \
         with the user's own authority there; use each as its library documents. Importing the \
         same library here gives a local copy that knows nothing of them, and `help()` on that \
         copy's classes describes the library without asking the host.\n",
    );
    for binding in bindings {
        let _ = write!(
            text,
            "\n- `{}`: {}",
            binding.name,
            binding.description.trim()
        );
        if binding.serialize {
            text.push_str(" Runs one call at a time.");
        }
    }
    let _ = write!(
        text,
        "\n\nEvery attribute read, call, iteration step, `len`, `repr` and comparison on one \
         is a request to the host, and it blocks your event loop until the host answers: no \
         message arrives and no timer fires meanwhile. For a long call, `await \
         asyncio.to_thread(lambda: {example}.f(x))` runs the whole expression on a worker thread \
         and keeps the loop turning. `runtime.bindings` lists them."
    );
    text
}

/// The line that opens a round: what waits, told by `announcement`, and how
/// many earlier turns its model calls are not sent, which the orientation
/// promises to say.
pub(crate) fn opening(announcement: &str, omitted: usize) -> String {
    match omitted {
        0 => format!("[outrig] {announcement}."),
        1 => {
            format!("[outrig] {announcement}. 1 earlier turn is not shown; runtime.history has it.")
        }
        n => format!(
            "[outrig] {announcement}. {n} earlier turns are not shown; runtime.history has them."
        ),
    }
}
