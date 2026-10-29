//! What the model is told before anything else: the system prompt's opening,
//! ahead of whatever preamble the agent's config supplies and of the
//! repository's own instructions for agents, which follow that.
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
//!
//! So are the session's skills, for the primary agent alone, each by name and
//! description as the Agent Skills standard lists them, and no more of each:
//! no skill's file is in the container until it is used, so the agent cannot
//! find them by looking. A child is told none; it can still use every one.

use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::history::Window;
use super::instructions::Instructions;
use crate::harness::bindings::Brief;
use crate::harness::event::InputBrief;
use crate::harness::skills::{Catalog, SkillBrief};

/// What every agent is told first: how it acts.
const ACT: &str = "You act on this project by writing Python. Your one tool, `submit_python`, runs \
                   code in a persistent interpreter inside the project's container; it is the \
                   only way to read a file, run a command, or change anything.\n\n";

/// What a session holds for its primary agent besides the interpreter: its
/// bindings and its skills, each in the session's order, and the catalog the
/// skills are of, which a directive the user sends names one of.
pub(crate) struct Holdings<'a> {
    pub(crate) bindings: &'a [Brief],
    pub(crate) skills: &'a [SkillBrief],
    pub(crate) catalog: Option<&'a Arc<Catalog>>,
}

/// The orientation, then `configured` after a blank line when there is one,
/// then the repository's `instructions` after another.
///
/// `workspace` is where the interpreter's working directory is: the primary's
/// `-w`, which is its workspace mount. Without one there is nothing true to
/// say about it, so the line is left out. `holdings` are the session's
/// bindings and skills; with none of either, nothing is said of it. `window`
/// is what each model call is sent of the conversation.
pub(crate) fn preamble(
    workspace: Option<&Path>,
    holdings: Holdings<'_>,
    configured: Option<&str>,
    instructions: Option<&Instructions>,
    window: Window,
) -> String {
    let mut text = String::from(ACT);
    text.push_str(
        "The user reaches you through messages on `runtime.channels[\"user\"]`. You are told how \
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
         code, what it awaits is cancelled; `runtime.wait` leaves those tasks running. Wait \
         with it, with a `timeout=`, for anything that may not return -- tests, builds, \
         network calls.\n\n",
    );
    text.push_str(&rest(
        workspace,
        holdings.bindings,
        window,
        "each time you are told messages are waiting",
    ));
    if !holdings.skills.is_empty() {
        text.push_str(&skilled(holdings.skills));
    }
    if let Some(configured) = configured {
        text.push_str("\n\n");
        text.push_str(configured);
    }
    if let Some(instructions) = instructions {
        text.push_str("\n\n");
        text.push_str(&instructions.section());
    }
    text
}

/// What every agent is told after how it is reached: its conversation, a
/// round beginning `begins`, its working directory, the interpreter, and the
/// session's bindings.
fn rest(workspace: Option<&Path>, bindings: &[Brief], window: Window, begins: &str) -> String {
    let Window { first, recent } = window;
    let mut text = format!(
        "Your whole conversation is in `runtime.history.turns`, a `Turn` for each of your model \
         calls, holding what you wrote and each call's source and result. Reading it from code \
         costs no context: only what you print is seen. Each call is sent less than all of it: \
         the first {first} rounds, the {recent} before this one, this one, and any turn you name \
         with `runtime.context.promote(turn)` until `runtime.context.demote(turn)`. Each call is \
         also held to the model's context window, which can leave out more, never your latest \
         call. A round begins {begins}, and its opening line says how many earlier turns are \
         left out.\n\n"
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
    if !bindings.is_empty() {
        text.push_str(&hosted(bindings));
    }
    text
}

/// What a child agent is: the one that works on submissions, or the one that
/// answers requests on its channels -- an agent class instance's among them,
/// which has a round as it launches -- each with what its instructions say of
/// them.
pub(crate) enum ChildBrief<'a> {
    Work,
    Request(&'a [ChannelText]),
    Instance(&'a [ChannelText]),
}

/// One of a request child's channels, as its instructions describe it.
pub(crate) struct ChannelText {
    pub(crate) name: String,
    pub(crate) doc: Option<String>,
    pub(crate) request_type: String,
    pub(crate) reply_type: String,
    pub(crate) request: String,
    pub(crate) reply: String,
}

/// A child agent's system prompt: how it acts, what it is, `prompt` -- the
/// first part of its instructions, which its parent wrote -- what it was
/// given, and how it answers; then what every agent is told. No user reaches
/// a child, and none of the session's own preamble applies to it.
pub(crate) fn child_preamble(
    workspace: Option<&Path>,
    bindings: &[Brief],
    window: Window,
    prompt: &str,
    inputs: &[InputBrief],
    brief: &ChildBrief<'_>,
) -> String {
    let mut text = String::from(ACT);
    text.push_str(
        "You are a child agent: another agent's code launched you to do work for it, and that \
         code is who you answer. No user reaches you, and you have no \
         `runtime.channels[\"user\"]`.\n\n",
    );
    text.push_str(prompt.trim());
    text.push_str("\n\n");
    if !inputs.is_empty() {
        text.push_str("These names were bound for you before you began:\n");
        for input in inputs {
            let _ = writeln!(
                text,
                "- `{}` ({}): {}",
                input.name, input.type_name, input.preview
            );
        }
        text.push('\n');
    }
    let begins = match brief {
        ChildBrief::Work => {
            text.push_str(
                "Your work arrives as submissions. A round opens on each one, naming it, binding \
                 its inputs as variables, and giving the type of the result it asks for, whose \
                 class -- and every dataclass it uses -- is bound under its name. Complete it with \
                 `await runtime.complete(value)`, `value` an instance of that type or its JSON \
                 form. It returns once the value is accepted, which ends the submission; one that \
                 is not accepted raises what is wrong, each problem with its path, so correct it \
                 and complete again. A round that ends with the submission open is followed by \
                 one more, and if that one ends with it open too, the submission fails. `await \
                 runtime.channels[\"progress\"].send(text)` tells your caller how far you have \
                 got, and settles nothing.\n\n\
                 `runtime.wait` is `asyncio.wait`: the same arguments, the same `(done, pending)` \
                 back, and a timeout that cancels nothing.\n\n",
            );
            "each time a submission is given to you, or one stays open"
        }
        ChildBrief::Request(channels) | ChildBrief::Instance(channels) => {
            text.push_str("You answer requests, which arrive on these channels:\n");
            for channel in *channels {
                let _ = write!(text, "\n- `runtime.channels[{:?}]`", channel.name);
                if let Some(doc) = &channel.doc {
                    let _ = write!(text, ": {}", doc.trim());
                }
                let _ = write!(
                    text,
                    "\n  A request is a `{}`:\n{}\n  Its reply is a `{}`:\n{}\n",
                    channel.request_type,
                    indented(&channel.request),
                    channel.reply_type,
                    indented(&channel.reply)
                );
            }
            text.push_str(
                "\nTake a request with `d = await runtime.channels[name].receive()`: `d.body` is \
                 the request and `d.id` its id. Answer it with `await d.reply(value)`, `value` an \
                 instance of the reply type -- bound under its name, with every dataclass it \
                 uses -- or its JSON form. It returns once the reply is accepted; one that is not \
                 raises what is wrong, each problem with its path, so correct it and reply again. \
                 A request you cannot answer you end with `await d.fail(message)`, saying why. \
                 You have no `runtime.complete`.\n\n\
                 `runtime.wait` is `asyncio.wait` -- the same arguments, the same `(done, pending)` \
                 back, and a timeout that cancels nothing -- except that while a request waits \
                 unread, it raises `runtime.MessageAvailable`, which `except Exception` does not \
                 catch. The request stays queued and the tasks keep running.\n\n",
            );
            match brief {
                ChildBrief::Instance(_) => {
                    "as you are launched, to get ready in, then when requests are waiting, or \
                     one you took is unanswered"
                }
                _ => "when requests are waiting, or one you took is unanswered",
            }
        }
    };
    text.push_str(&rest(workspace, bindings, window, begins));
    text
}

/// `schema`, each line indented to sit under a list item.
fn indented(schema: &str) -> String {
    schema
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
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

/// The most skills the preamble lists.
const SKILLS_LISTED_MAX: usize = 50;

/// The most bytes of the preamble's list of skills: some 3,000 tokens.
const SKILLS_TEXT_MAX: usize = 12 * 1024;

/// What the primary agent is told of the session's skills, of which there is
/// at least one: as many as fit [`SKILLS_LISTED_MAX`] and
/// [`SKILLS_TEXT_MAX`], each by name and description, then how many more
/// there are and where they are listed.
fn skilled(skills: &[SkillBrief]) -> String {
    let mut text = String::from(
        "\n\nThese skills come with the session, each instructions for a kind of work: \
         `outrig.skills.read(name)` returns them, to read when a skill's description fits what \
         you are doing. One that names a package also has Python, its entry module that \
         package's `skill`, which you import as any module. `help(outrig.skills)` says more.\n\n\
         A message from the user whose delivery names a skill, as `d.skill`, is the user typing \
         `/name`: it asks you to run that skill. For a skill with a package, call `await \
         outrig.skills.invoke(d.skill, (d.body.split(maxsplit=1) + [\"\"])[1])`, the text after \
         the name and the whitespace that ends it. When that raises `outrig.skills.SkillNeedsClarification`, send its `question` to \
         the user on `runtime.channels[\"user\"]`, and once they answer, call `invoke` again \
         with `params=outrig.skills.params(d.skill)(...)`. For a skill without one, the message \
         is its instructions followed by what the user typed: follow them.\n",
    );
    let mut listed = 0;
    let mut size = 0;
    for skill in skills {
        let mut line = format!("\n- `{}`", skill.name);
        if let Some(package) = &skill.package {
            let _ = write!(line, " (`{package}`)");
        }
        let _ = write!(line, ": {}", skill.description);
        if listed == SKILLS_LISTED_MAX || size + line.len() > SKILLS_TEXT_MAX {
            break;
        }
        size += line.len();
        listed += 1;
        text.push_str(&line);
    }
    let more = skills.len() - listed;
    if more > 0 {
        let _ = write!(
            text,
            "\n\n{more} more {} not listed here; `outrig.skills.catalog()` has every one.",
            if more == 1 { "is" } else { "are" }
        );
    }
    text
}

/// The text that opens a child's round: `lead`, what the round is for, with
/// how many earlier turns its model calls are not sent, then `detail`.
pub(crate) fn child_opening(lead: &str, detail: &str, omitted: usize) -> String {
    let mut text = opening(lead, omitted);
    if !detail.is_empty() {
        text.push_str("\n\n");
        text.push_str(detail);
    }
    text
}

/// What an agent class instance's child is told as its first round opens, as
/// it launches: the round is for getting ready.
pub(crate) const ORIENTING: &str = "you have just been launched, and this round is for getting \
                                    ready: read your instructions, and prepare what answering \
                                    requests will need";

/// What a work child is told as a submission is given to it.
pub(crate) fn submission(
    id: u64,
    inputs: &[InputBrief],
    result_type: &str,
    schema: &str,
) -> (String, String) {
    let lead = format!("submission {id} is yours to complete");
    let mut detail = String::new();
    if inputs.is_empty() {
        detail.push_str("It has no inputs.\n");
    } else {
        detail.push_str("Its inputs are bound as variables:\n");
        for input in inputs {
            let _ = writeln!(
                detail,
                "- `{}` ({}): {}",
                input.name, input.type_name, input.preview
            );
        }
    }
    let _ = write!(
        detail,
        "Complete it with `await runtime.complete(value)`, `value` a `{result_type}`:\n{schema}"
    );
    (lead, detail)
}

/// What a work child is told when its submission's round ended without
/// completing it: the one more round it gets.
pub(crate) fn nudge(id: u64, result_type: &str) -> String {
    format!(
        "submission {id} is still open: your last round ended without completing it. Complete \
         it now with `await runtime.complete(value)`, `value` a `{result_type}`; if this round \
         ends without that too, the submission fails"
    )
}

/// What a request child is told as a round opens on its requests: how many
/// wait unread on each channel, in the announcer's words, and each it took
/// and has not answered, with how long ago.
pub(crate) fn requests(unread: &str, taken: &[(u64, String, Duration)]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !unread.is_empty() {
        parts.push(unread.to_string());
    }
    for (id, channel, waited) in taken {
        parts.push(format!(
            "you took request {id} from runtime.channels[{channel:?}] {} ago and have not \
             answered it",
            human(*waited)
        ));
    }
    parts.join("; ")
}

/// `duration`, to the second, as a person reads it.
fn human(duration: Duration) -> String {
    let secs = duration.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
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
