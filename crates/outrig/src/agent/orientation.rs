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
//! (`plan/phase/0003-python/discovery.md`). A child given facilities is told
//! of those in their place, and of no other binding
//! (`plan/phase/0003-python/facilities.md`).
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
use crate::harness::event::{FacilityBrief, FacilityKind, InputBrief};
use crate::harness::services::ServiceBrief;
use crate::harness::skills::{Catalog, SkillBrief};

/// What every agent is told first: how it acts.
const ACT: &str = "You act on this project by writing Python. Your one tool, `submit_python`, runs \
                   code in a persistent interpreter inside the project's container; it is the \
                   only way to read a file, run a command, or change anything.\n\n";

/// What a session holds for its primary agent besides the interpreter: its
/// bindings, its services and its skills, each in the session's order, and the
/// catalog the skills are of, which a directive the user sends names one of.
pub(crate) struct Holdings<'a> {
    pub(crate) bindings: &'a [Brief],
    pub(crate) services: &'a [ServiceBrief],
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
         takes the next one, whose `.body` is its text, and `runtime.channels[\"user\"].pending()` \
         counts them. `receive()` removes the message it takes. With none waiting it does not \
         return until the user sends one, and the `submit_python` call that awaits it does not \
         finish until then. `await runtime.channels[\"user\"].send(text)` sends the user a \
         message. What you write yourself is commentary; send what you mean the user to have. \
         To wait for the user, stop writing -- reply without calling `submit_python` -- and a new \
         round begins when their next message arrives. A send is also the only way code still \
         running after you stop writing can reach them.\n\n\
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
        View::Session {
            bindings: holdings.bindings,
            services: holdings.services,
        },
        window,
        "each time you are told messages are waiting",
    ));
    if !holdings.skills.is_empty() {
        text.push_str("\n\n");
        text.push_str(&skilled(holdings.skills, true));
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

/// What a child that completes submissions is told of `runtime.wait`.
const WAIT: &str = "`runtime.wait` is `asyncio.wait`: the same arguments, the same `(done, pending)` \
                    back, and a timeout that cancels nothing.\n\n";

/// What every child is told of ending a round, which no request outlives
/// for long.
const NOT_WAITING: &str = "Ending your round is not waiting: nothing opens a round when what you \
                           waited on finishes, so wait inside the round, with `runtime.wait`.";

/// What the warning before a cancel says of the round that ended.
const NOT_A_WAIT: &str = "A round's end is not a wait: nothing opens a round when what you waited \
                          on finishes.";

/// What an agent is told it acts through besides the interpreter.
#[derive(Clone, Copy)]
pub(crate) enum View<'a> {
    /// The session's bindings and services, every one.
    Session {
        bindings: &'a [Brief],
        services: &'a [ServiceBrief],
    },
    /// The facilities its constructor gave it, or that it inherited from its
    /// parent, and no other binding or service.
    Given(&'a [Given]),
}

/// One facility a child was given, as its instructions describe it: what its
/// events say of it, its description -- the operator's for a binding, the
/// first line of its class's docstring for an object -- and whether a binding
/// runs one call at a time.
pub(crate) struct Given {
    pub(crate) brief: FacilityBrief,
    pub(crate) description: String,
    pub(crate) serialize: bool,
}

/// What every agent is told after how it is reached: its conversation, a
/// round beginning `begins`, its working directory, the interpreter, and what
/// it acts through.
fn rest(workspace: Option<&Path>, view: View<'_>, window: Window, begins: &str) -> String {
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
    match view {
        View::Session { bindings, services } if !bindings.is_empty() || !services.is_empty() => {
            text.push_str(&held(bindings, services));
        }
        View::Given(given) if !given.is_empty() => text.push_str(&facility_rules(given)),
        _ => {}
    }
    text
}

/// What a child agent is: the one that works on submissions, or the one that
/// answers requests on its channels -- an agent class instance's among them,
/// which has a round as it launches, and has the instance as its `self` unless
/// a host program constructed it -- each with what its instructions say of
/// them.
pub(crate) enum ChildBrief<'a> {
    Work,
    /// The one child of one typed call, which is that call.
    Call {
        signature: &'a str,
        result_type: &'a str,
        schema: &'a str,
    },
    Request(&'a [ChannelText]),
    /// An agent class instance's child.
    Instance {
        channels: &'a [ChannelText],
        /// When it has the instance as `self`, the class's `__qualname__` and
        /// the names of the aliases bound beside it.
        own: Option<(&'a str, &'a [&'a str])>,
        /// The titled sections its class contributed, each a title and a
        /// text, which follow its docstring.
        sections: &'a [(&'a str, &'a str)],
        /// Whether its parent is the session's owner: it is a root, which no
        /// agent's code made and no user reaches.
        root: bool,
        /// The session's skills, for a root its owner constructed in its own
        /// kernel; none for any other.
        skills: &'a [SkillBrief],
    },
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

/// What a child that is not a root is told of what it is.
const CHILD: &str = "You are a child agent: another agent's code launched you to do work for it, and \
                     that code is who you answer.\n\n";

/// What a root is told of what it is: the program that owns the session
/// constructed it, and no agent's code did.
const ROOT: &str = "You are a root agent: the program that owns this session constructed you; no \
                    user reaches you, and that program is who you answer.\n\n";

/// A child agent's system prompt, in this order: how it acts, what it is,
/// `prompt` -- the first part of its instructions, which its parent wrote, or
/// its class's docstring -- the sections its class contributed, what it was
/// given, what it is of its parent's, the facilities it was given, and how it
/// answers; then what every agent is told. No user reaches a child, and none
/// of the session's own preamble applies to it.
pub(crate) fn child_preamble(
    workspace: Option<&Path>,
    view: View<'_>,
    window: Window,
    prompt: &str,
    inputs: &[InputBrief],
    brief: &ChildBrief<'_>,
) -> String {
    let mut text = String::from(ACT);
    let root = matches!(brief, ChildBrief::Instance { root: true, .. });
    if let ChildBrief::Call { signature, .. } = brief {
        let _ = write!(
            text,
            "You are a child agent: another agent's code called `{signature}`, and you are that \
             call. The function's docstring is your instructions, indented below:\n\n{}\n\n",
            indented(prompt.trim())
        );
    } else {
        text.push_str(if root { ROOT } else { CHILD });
        text.push_str(prompt.trim());
        text.push_str("\n\n");
    }
    if let ChildBrief::Instance { sections, .. } = brief {
        for (title, body) in *sections {
            let _ = write!(text, "## {title}\n\n{body}\n\n");
        }
    }
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
    if let ChildBrief::Instance {
        own: Some((qualname, aliases)),
        ..
    } = brief
    {
        let (maker, asker) = if root {
            (
                "that program constructed in this interpreter for you",
                "that program",
            )
        } else {
            ("your parent's code constructed", "your parent")
        };
        let _ = write!(
            text,
            "`self` is the `{qualname}` instance {maker}: the same object, not a copy. Its \
             attributes hold what its constructor stored, and its ordinary methods run here when \
             you call them; its request methods are how {asker} asks you for something, so \
             calling one on `self` raises `TypeError`."
        );
        for alias in *aliases {
            let _ = write!(
                text,
                " `{alias}` is the `{alias}` its constructor was given, the same object."
            );
        }
        text.push_str(if root {
            " Keep your own state on `self`, not in a module's globals, which every agent that \
             imports the module shares. You run until that program releases you, or as the \
             session ends.\n\n"
        } else {
            " Your parent's code holds `self` too, and can change it while yours runs: one \
             attribute read or write is whole, a read followed by a write is not. Keep your own \
             state on `self`, not in a module's globals, which every agent that imports the \
             module shares. You run until you are released -- by your parent, with it, or as the \
             session ends.\n\n"
        });
    }
    if let View::Given(given) = view
        && !given.is_empty()
    {
        text.push_str(&facility_entries(given));
    }
    let begins = match brief {
        ChildBrief::Call {
            result_type,
            schema,
            ..
        } => {
            let _ = write!(
                text,
                "Your round opens on the call's submission, naming it and binding each argument as \
                 a variable of its parameter's name. Complete it with `await \
                 runtime.complete(value)`, `value` a `{result_type}` -- bound under that name, \
                 with every dataclass it uses -- or its JSON form:\n\n{}\n\n\
                 It returns once the value is accepted, which ends the call; nothing more is \
                 asked of you. One that is not accepted raises what is wrong, each problem with \
                 its path, so correct it and complete again. A call you cannot complete you end \
                 with `await runtime.fail(message)`, saying why. {NOT_WAITING} A round that ends \
                 with the submission open is followed by up to two more, the last warning that \
                 the call is cancelled if it ends the same way. `await \
                 runtime.channels[\"progress\"].send(text)` tells your caller how far you have \
                 got, and settles nothing.\n\n{WAIT}",
                indented(schema)
            );
            "on the call's submission, and up to twice more while rounds leave it open"
        }
        ChildBrief::Work => {
            let _ = write!(
                text,
                "Your work arrives as submissions. A round opens on each one, naming it, binding \
                 its inputs as variables, and giving the type of the result it asks for, whose \
                 class -- and every dataclass it uses -- is bound under its name. Complete it with \
                 `await runtime.complete(value)`, `value` an instance of that type or its JSON \
                 form. It returns once the value is accepted, which ends the submission; one that \
                 is not accepted raises what is wrong, each problem with its path, so correct it \
                 and complete again. A submission you cannot complete you end with `await \
                 runtime.fail(message)`, saying why. {NOT_WAITING} A round that ends with the \
                 submission open is followed by up to two more, the last warning that it is \
                 cancelled if it ends the same way. `await \
                 runtime.channels[\"progress\"].send(text)` tells your caller how far you have \
                 got, and settles nothing.\n\n",
            );
            text.push_str(WAIT);
            "each time a submission is given to you, or one stays open"
        }
        // A root that declares no request channel has none to answer on, and says so.
        ChildBrief::Instance {
            root: true,
            channels: [],
            ..
        } => {
            let _ = write!(
                text,
                "Your class declares no request channel, so no request arrives and you have no \
                 `runtime.complete`. {WAIT}{NOT_WAITING}\n\n"
            );
            "as you are launched"
        }
        ChildBrief::Request(channels) | ChildBrief::Instance { channels, .. } => {
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
                 the request and `d.id` its id. A round's opening line says what waits, by channel \
                 and count, and never what it holds: you read each request in code, with \
                 `receive()`. Answer it with `await d.reply(value)`, `value` an \
                 instance of the reply type -- bound under its name, with every dataclass it \
                 uses -- or its JSON form. It returns once the reply is accepted; one that is not \
                 raises what is wrong, each problem with its path, so correct it and reply again. \
                 A request you cannot answer you end with `await d.fail(message)`, saying why. \
                 Only `d.reply` and `d.fail` answer: what you write yourself is not delivered to \
                 the request's sender. You have no `runtime.complete`.\n\n\
                 `runtime.wait` is `asyncio.wait` -- the same arguments, the same `(done, pending)` \
                 back, and a timeout that cancels nothing -- except that while a request waits \
                 unread, it raises `runtime.MessageAvailable`, which `except Exception` does not \
                 catch. The request stays queued and the tasks keep running.\n\n",
            );
            let _ = write!(
                text,
                "{NOT_WAITING} Code you leave running when you stop writing keeps no request \
                 open. A round that ends with a request unread or unanswered is followed by up to \
                 two more, the last warning that the requests are cancelled if it ends the same \
                 way, and each sender then gets an exception.\n\n"
            );
            match brief {
                ChildBrief::Instance { .. } => {
                    "as you are launched, to get ready in, then when requests are waiting, or \
                     one you took is unanswered"
                }
                _ => "when requests are waiting, or one you took is unanswered",
            }
        }
    };
    // A root the owner constructed is the application's primary, and is told the session's skills.
    if let ChildBrief::Instance { skills, .. } = brief
        && !skills.is_empty()
    {
        text.push_str(&skilled(skills, false));
        text.push_str("\n\n");
    }
    text.push_str(&rest(workspace, view, window, begins));
    text
}

/// `schema`, each line indented to sit under a list item.
fn indented(schema: &str) -> String {
    schema
        .lines()
        .map(|line| match line {
            "" => String::new(),
            _ => format!("    {line}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the agent is told of the session's bindings and services, of which
/// there is at least one: each by name and description, and how they are
/// used, with the first one's name standing in for any of them.
fn held(bindings: &[Brief], services: &[ServiceBrief]) -> String {
    let mut text = String::new();
    if !bindings.is_empty() {
        text.push_str(
            "\n\nThese names are objects held by processes on the host, outside this container, \
             with the user's own authority there; use each as its library documents. Importing \
             the same library here gives a local copy that knows nothing of them, and `help()` on \
             that copy's classes describes the library without asking the host.\n",
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
    }
    if !services.is_empty() {
        let _ = writeln!(
            text,
            "\n\nThese names are services the host runs, outside this container: each is \
             {SERVICE_IS}, and `help(name)` describes its methods."
        );
        for service in services {
            let _ = write!(
                text,
                "\n- `{}`: {}",
                service.name,
                service.description.trim()
            );
        }
    }
    text.push_str("\n\n");
    text.push_str(&rules(
        bindings.first().map(|binding| binding.name.as_str()),
        services.first().map(|service| service.name.as_str()),
    ));
    text.push_str(
        " `runtime.bindings` lists them, and `outrig.bindings[name]` reaches each wherever its \
         name holds something else: ",
    );
    text.push_str(match services.is_empty() {
        true => "a binding never replaces",
        false => "none replaces",
    });
    text.push_str(" a name your code bound before it came, nor a builtin.");
    text
}

/// What a service is, as an agent holding one is told.
const SERVICE_IS: &str = "an object whose methods are calls to the host, by value -- the \
                          arguments and the result are JSON data --";

/// The rules about bindings and services that looking cannot teach, for an
/// agent holding `binding` and `service`, the first of each, of which there is
/// at least one: that a use of one blocks the event loop until the host
/// answers, with the first standing in for any; and, with a service, that a
/// long one hands back a ticket, which the mechanism cannot enforce.
fn rules(binding: Option<&str>, service: Option<&str>) -> String {
    let lead = match (binding, service) {
        (Some(_), None) => {
            "Every attribute read, call, iteration step, `len`, `repr` and comparison on one is a \
             request to the host"
        }
        (Some(_), Some(_)) => {
            "Every attribute read, call, iteration step, `len`, `repr` and comparison on a \
             binding, and every call of a service's method, is a request to the host"
        }
        (None, _) => "Every call of a service's method is a request to the host",
    };
    let example = binding.or(service).unwrap_or_default();
    let mut text = format!(
        "{lead}, and it blocks your event loop until the host answers: no message arrives and no \
         timer fires meanwhile. For a long call, `await asyncio.to_thread(lambda: {example}.f(x))` \
         runs the whole expression on a worker thread and keeps the loop turning."
    );
    if service.is_some() {
        text.push_str(
            " A service's method returns a value at once, so one that starts long work hands back \
             a ticket: wait for its completion with `runtime.wait`, never inside a call.",
        );
    }
    text
}

/// What a child given facilities is told of them, of which there is at least
/// one, before its channels: each by name, kind and description.
fn facility_entries(given: &[Given]) -> String {
    let mut text = String::from("You were given these to act through, by name:\n");
    for facility in given {
        let (brief, description) = (&facility.brief, facility.description.trim());
        match brief.kind {
            FacilityKind::Binding => {
                let _ = write!(
                    text,
                    "\n- `{}`, a binding the host holds: {description}",
                    brief.name
                );
                if facility.serialize {
                    text.push_str(" Runs one call at a time.");
                }
            }
            FacilityKind::Service => {
                let _ = write!(
                    text,
                    "\n- `{}`, a service the host runs: {description}",
                    brief.name
                );
            }
            _ => {
                let _ = write!(
                    text,
                    "\n- `{}`, a `{}` in this interpreter",
                    brief.name, brief.type_name
                );
                if !description.is_empty() {
                    let _ = write!(text, ": {description}");
                }
            }
        }
    }
    text.push_str("\n\n");
    text
}

/// What a child given facilities is told of how to use them, with what every
/// agent is told last, of which there is at least one: how each kind among
/// them is used, the rule about a binding or a service among them, and that
/// the mapping is read as it stands while its globals and these entries were
/// made once.
fn facility_rules(given: &[Given]) -> String {
    let mut text = String::from("\n\n");
    let first = |kind| {
        given
            .iter()
            .find(|f| f.brief.kind == kind)
            .map(|f| f.brief.name.as_str())
    };
    let (binding, service) = (first(FacilityKind::Binding), first(FacilityKind::Service));
    if binding.is_some() {
        text.push_str(
            "A binding is an object held by a process on the host, outside this container, with \
             the user's own authority there; use it as its library documents. ",
        );
    }
    if service.is_some() {
        let _ = write!(
            text,
            "A service is {SERVICE_IS} and `help(name)` describes its methods. "
        );
    }
    if binding.is_some() || service.is_some() {
        text.push_str(&rules(binding, service));
        text.push(' ');
    }
    if given.iter().any(|f| f.brief.kind == FacilityKind::Object) {
        text.push_str(
            "An object is plain Python, shared with the agent that gave it to you, and a call on \
             it runs here. ",
        );
    }
    text.push_str(
        "Each is a global of its name unless your code bound that name first, and \
         `outrig.bindings[name]` reaches each wherever its name holds something else. \
         `runtime.bindings` lists your facilities as they stand: one given after you began is \
         reached as `outrig.bindings[name]`, never as a global. `outrig.bindings` reaches no \
         other.",
    );
    text
}

/// What an agent made before a host program injected `brief`'s object is told of it.
pub(crate) fn injected(brief: &Brief) -> String {
    let name = &brief.name;
    format!(
        "`outrig.bindings[{name:?}]` is now bound: {} The global `{name}` is it too, unless your \
         code had bound that name, or it is a builtin's",
        brief.description.trim()
    )
}

/// The most skills the preamble lists.
const SKILLS_LISTED_MAX: usize = 50;

/// The most bytes of the preamble's list of skills: some 3,000 tokens.
const SKILLS_TEXT_MAX: usize = 12 * 1024;

/// What an agent is told of the session's skills, of which there is at least
/// one: as many as fit [`SKILLS_LISTED_MAX`] and [`SKILLS_TEXT_MAX`], each by
/// name and description, then how many more there are and where they are
/// listed. With `directive`, how the user's `/name` message asks for one,
/// which only the primary, with a user channel, is told.
fn skilled(skills: &[SkillBrief], directive: bool) -> String {
    let mut text = String::from(
        "These skills come with the session, each instructions for a kind of work: \
         `outrig.skills.read(name)` returns them, to read when a skill's description fits what \
         you are doing. One that names a package also has Python, its entry module that \
         package's `skill`, which you import as any module. `help(outrig.skills)` says more.\n",
    );
    if directive {
        text.push_str(
            "\nA message from the user whose delivery names a skill, as `d.skill`, is the user typing \
             `/name`: it asks you to run that skill. For a skill with a package, call `await \
             outrig.skills.invoke(d.skill, (d.body.split(maxsplit=1) + [\"\"])[1])`, the text after \
             the name and the whitespace that ends it. When that raises `outrig.skills.SkillNeedsClarification`, send its `question` to \
             the user on `runtime.channels[\"user\"]`, and once they answer, call `invoke` again \
             with `params=outrig.skills.params(d.skill)(...)`. For a skill without one, the message \
             is its instructions followed by what the user typed: follow them.\n",
        );
    }
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
const ORIENTING: &str = "you have just been launched, and this round is for getting ready: read \
                         your instructions, and prepare what answering requests will need";

/// What a root whose class contributed sections is told besides: its task is
/// in its instructions, and this round is where it begins.
const ORIENTING_TASK: &str =
    "Your task is in your instructions, and this round is where you begin it";

/// What an agent class instance's first round opens on, as it launches: that
/// it is for getting ready, and for a root with a `task` in its instructions
/// that it is where the task begins; then `waiting`, by channel and count, when
/// a request already waits. What waits is never told in the instructions, nor
/// its body here: the round reads it in code.
pub(crate) fn orienting(task: bool, waiting: &str) -> String {
    let mut lead = String::from(ORIENTING);
    if task {
        let _ = write!(lead, ". {ORIENTING_TASK}");
    }
    if !waiting.is_empty() {
        let _ = write!(lead, ". {waiting}");
    }
    lead
}

/// What a work child is told as a submission is given to it.
pub(crate) fn submission(
    id: u64,
    inputs: &[InputBrief],
    result_type: &str,
    schema: &str,
) -> (String, String) {
    // A typed call's schema is in its instructions, which call its work the call's submission.
    let call = schema.is_empty();
    let which = if call {
        "the call's submission"
    } else {
        "submission"
    };
    let lead = format!("{which} {id} is yours to complete");
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
    if !call {
        let _ = write!(
            detail,
            "Complete it with `await runtime.complete(value)`, `value` a `{result_type}`:\n{schema}"
        );
    }
    (lead, detail)
}

/// What a work child is told when its submission's round ended without
/// completing it: a round that asks for it.
pub(crate) fn nudge(id: u64, result_type: &str) -> String {
    format!(
        "submission {id} is still open: your last round ended without completing it. Complete \
         it now with `await runtime.complete(value)`, `value` a `{result_type}`"
    )
}

/// What a work child is told as a round opens on submission `id`, which its
/// last two rounds left open: each way to end it, that a round's end is not a
/// wait, and that ending this round with it open cancels it.
pub(crate) fn submission_warning(id: u64, result_type: &str) -> (String, String) {
    (
        format!("submission {id} is still open: your last two rounds ended without completing it"),
        format!(
            "Complete it with `await runtime.complete(value)`, `value` a `{result_type}`; end it \
             with `await runtime.fail(message)`, saying why, if you cannot; or wait for what you \
             need with `runtime.wait`. {NOT_A_WAIT} If this round ends with it open, it is \
             cancelled, and its caller gets an exception."
        ),
    )
}

/// What a request child is told as a round opens on what its last round left
/// as it was: `outstanding`, in [`requests`]' words; that a round's end is not
/// a wait; each way out, a receive first on each channel `unread_on` names,
/// where requests wait unread; and that ending this round so cancels them.
pub(crate) fn requests_warning(outstanding: &str, unread_on: &[String]) -> (String, String) {
    let mut detail = format!(
        "{NOT_A_WAIT} Answer a request with `await d.reply(value)`, or end one you cannot answer \
         with `await d.fail(message)`. "
    );
    if !unread_on.is_empty() {
        let receives: Vec<String> = unread_on
            .iter()
            .map(|channel| format!("`await runtime.channels[{channel:?}].receive()`"))
            .collect();
        let _ = write!(
            detail,
            "Take an unread one first, with {}: `runtime.wait` raises \
             `runtime.MessageAvailable` while one waits unread. ",
            receives.join(" or ")
        );
    }
    detail.push_str(
        "Wait for what you need with `runtime.wait`. If this round ends with any of them \
         unanswered, they are cancelled, and each sender gets an exception.",
    );
    (
        format!("your last round ended with requests unanswered: {outstanding}"),
        detail,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn preamble(brief: &ChildBrief<'_>, inputs: &[InputBrief]) -> String {
        child_preamble(
            None,
            View::Session {
                bindings: &[],
                services: &[],
            },
            Window::DEFAULT,
            "Name the capital.",
            inputs,
            brief,
        )
    }

    /// An agent class instance's child that contributed no section and is no root.
    fn instance<'a>(
        channels: &'a [ChannelText],
        own: Option<(&'a str, &'a [&'a str])>,
    ) -> ChildBrief<'a> {
        ChildBrief::Instance {
            channels,
            own,
            sections: &[],
            root: false,
            skills: &[],
        }
    }

    fn given(name: &str, kind: FacilityKind, type_name: &str, description: &str) -> Given {
        Given {
            brief: FacilityBrief {
                name: name.into(),
                kind,
                target: None,
                type_name: type_name.into(),
            },
            description: description.into(),
            serialize: false,
        }
    }

    /// A child given facilities is told of each by the name it was given under, with its
    /// kind and description, and of no session binding; the blocking rule comes with a binding
    /// among them, and with none it is not said. An empty mapping says nothing.
    #[test]
    fn a_child_given_facilities_is_told_of_them_and_no_other_binding() {
        fn told(view: View<'_>) -> String {
            child_preamble(
                None,
                view,
                Window::DEFAULT,
                "Review.",
                &[],
                &ChildBrief::Work,
            )
        }
        let mut files = given(
            "files",
            FacilityKind::Binding,
            "git.Repo",
            "The repository.",
        );
        files.serialize = true;
        let task = given(
            "task",
            FacilityKind::Object,
            "app.Task",
            "The task at hand.",
        );
        let text = told(View::Given(&[files, task]));
        for said in [
            "- `files`, a binding the host holds: The repository. Runs one call at a time.",
            "- `task`, a `app.Task` in this interpreter: The task at hand.",
            "`await asyncio.to_thread(lambda: files.f(x))`",
            "An object is plain Python",
            "one given after you began is reached as `outrig.bindings[name]`, never as a global",
        ] {
            assert!(text.contains(said), "{said}: {text}");
        }
        assert!(!text.contains("`repo`"), "{text}");

        let text = told(View::Given(&[given(
            "task",
            FacilityKind::Object,
            "app.Task",
            "",
        )]));
        assert!(
            text.contains("- `task`, a `app.Task` in this interpreter\n"),
            "{text}"
        );
        assert!(!text.contains("blocks your event loop"), "{text}");

        let none = told(View::Given(&[]));
        assert!(!none.contains("act through"), "{none}");
        let session = [Brief {
            name: "repo".into(),
            description: "The repository.".into(),
            type_name: "git.Repo".into(),
            serialize: true,
        }];
        let session = told(View::Session {
            bindings: &session,
            services: &[],
        });
        assert!(session.contains("- `repo`: The repository."), "{session}");
    }

    /// A session's services are listed beside its bindings, by name and description, with the
    /// blocking rule said once for both and the ticket sentence; a child given a service is told
    /// of it, and of the same rules, and one given none hears nothing of services.
    #[test]
    fn services_are_told_beside_bindings_with_the_blocking_rule_once() {
        fn told(view: View<'_>) -> String {
            child_preamble(
                None,
                view,
                Window::DEFAULT,
                "Review.",
                &[],
                &ChildBrief::Work,
            )
        }
        let bindings = [Brief {
            name: "repo".into(),
            description: "The repository.".into(),
            type_name: "git.Repo".into(),
            serialize: false,
        }];
        let services = [ServiceBrief {
            name: "tracker".into(),
            description: "The issue tracker.".into(),
        }];
        let both = told(View::Session {
            bindings: &bindings,
            services: &services,
        });
        for said in [
            "- `repo`: The repository.",
            "These names are services the host runs",
            "- `tracker`: The issue tracker.",
            "and every call of a service's method, is a request to the host",
            "hands back a ticket: wait for its completion with `runtime.wait`",
            "none replaces a name your code bound",
        ] {
            assert!(both.contains(said), "{said}: {both}");
        }
        assert_eq!(both.matches("blocks your event loop").count(), 1, "{both}");

        let alone = told(View::Session {
            bindings: &[],
            services: &services,
        });
        assert!(
            alone.contains("Every call of a service's method is a request to the host"),
            "{alone}"
        );
        assert!(
            alone.contains("`await asyncio.to_thread(lambda: tracker.f(x))`"),
            "{alone}"
        );
        assert!(!alone.contains("objects held by processes"), "{alone}");

        let given_one = told(View::Given(&[given(
            "svc",
            FacilityKind::Service,
            "outrig.services.tracker",
            "The issue tracker.",
        )]));
        for said in [
            "- `svc`, a service the host runs: The issue tracker.",
            "A service is an object whose methods are calls to the host",
            "`await asyncio.to_thread(lambda: svc.f(x))`",
            "hands back a ticket",
        ] {
            assert!(given_one.contains(said), "{said}: {given_one}");
        }
        let given_none = told(View::Given(&[given(
            "task",
            FacilityKind::Object,
            "app.Task",
            "",
        )]));
        assert!(!given_none.contains("service"), "{given_none}");
    }

    #[test]
    fn a_call_child_is_briefed_as_the_call() {
        let text = preamble(
            &ChildBrief::Call {
                signature: "city_fact(country: str) -> Fact",
                result_type: "Fact",
                schema: "{\n  \"type\": \"object\"\n}",
            },
            &[],
        );
        assert!(
            text.contains("called `city_fact(country: str) -> Fact`"),
            "{text}"
        );
        assert!(text.contains("    Name the capital."), "{text}");
        assert!(text.contains("`value` a `Fact`"), "{text}");
        assert!(
            text.contains("    {\n      \"type\": \"object\"\n    }"),
            "{text}"
        );
        assert!(!text.contains("channels[\"user\"]"), "{text}");
        assert!(!text.contains("submissions"), "{text}");
    }

    #[test]
    fn no_child_is_told_of_the_user_channel() {
        for brief in [ChildBrief::Work, ChildBrief::Request(&[])] {
            let text = preamble(&brief, &[]);
            assert!(!text.contains("channels[\"user\"]"), "{text}");
        }
    }

    #[test]
    fn a_call_opening_names_the_inputs_not_the_schema() {
        let inputs = [InputBrief {
            name: "country".into(),
            type_name: "str".into(),
            preview: "\"Japan\"".into(),
        }];
        let (lead, detail) = submission(4, &inputs, "Fact", "");
        assert_eq!(lead, "the call's submission 4 is yours to complete");
        assert!(detail.contains("`country` (str)"), "{detail}");
        assert!(!detail.contains("SCHEMA"), "{detail}");
        let (_, detail) = submission(4, &inputs, "Fact", "{SCHEMA}");
        assert!(detail.contains("SCHEMA"), "{detail}");
    }

    /// The first nudge asks for what is open and says nothing of what follows: a request
    /// child's names each request it took, by id, channel and wait, and counts the unread; a
    /// work child's asks for the result by the submission's id.
    #[test]
    fn the_first_nudge_asks_and_says_nothing_of_what_follows() {
        let taken = [(7, "fizz".to_string(), Duration::from_secs(12))];
        assert_eq!(
            requests(
                "2 requests are waiting on runtime.channels[\"fizz\"]",
                &taken
            ),
            "2 requests are waiting on runtime.channels[\"fizz\"]; you took request 7 from \
             runtime.channels[\"fizz\"] 12s ago and have not answered it"
        );
        assert_eq!(
            nudge(12, "Total"),
            "submission 12 is still open: your last round ended without completing it. Complete \
             it now with `await runtime.complete(value)`, `value` a `Total`"
        );
    }

    /// Each warning names what is outstanding and every way out, says a round's end is not a
    /// wait, and that ending this round so cancels. A request child is told to take an unread
    /// request first, on each channel that holds one.
    #[test]
    fn each_warning_names_what_is_open_every_way_out_and_the_cancel() {
        let (lead, detail) = requests_warning(
            "1 request is waiting on runtime.channels[\"fizz\"]; you took request 7 from \
             runtime.channels[\"fizz\"] 12s ago and have not answered it",
            &["fizz".to_string()],
        );
        assert!(
            lead.starts_with("your last round ended with requests unanswered: 1 request is")
                && lead.contains("you took request 7 from"),
            "{lead}"
        );
        for said in [
            "A round's end is not a wait",
            "`await d.reply(value)`",
            "`await d.fail(message)`",
            "`await runtime.channels[\"fizz\"].receive()`",
            "`runtime.wait` raises `runtime.MessageAvailable`",
            "with `runtime.wait`",
            "they are cancelled, and each sender gets an exception",
        ] {
            assert!(detail.contains(said), "{said}: {detail}");
        }
        let (_, several) = requests_warning("", &["a".to_string(), "b".to_string()]);
        assert!(
            several.contains(
                "`await runtime.channels[\"a\"].receive()` or \
                 `await runtime.channels[\"b\"].receive()`"
            ),
            "{several}"
        );
        let (_, taken_only) = requests_warning("", &[]);
        assert!(!taken_only.contains("receive()"), "{taken_only}");

        let (lead, detail) = submission_warning(12, "Total");
        assert_eq!(
            lead,
            "submission 12 is still open: your last two rounds ended without completing it"
        );
        for said in [
            "`await runtime.complete(value)`, `value` a `Total`",
            "`await runtime.fail(message)`",
            "with `runtime.wait`",
            "A round's end is not a wait",
            "it is cancelled, and its caller gets an exception",
        ] {
            assert!(detail.contains(said), "{said}: {detail}");
        }
    }

    /// A child that has its instance as `self` is told so after its inputs and before its
    /// channels: the class, that `self` is the object its parent's code constructed, that a
    /// request method on it raises, that the two share it, and that it runs until released; and
    /// each alias, as its constructor's argument of that name. A host program's instance, which
    /// has no `self`, is told nothing of one.
    #[test]
    fn a_child_is_told_of_its_self_and_its_aliases() {
        let channels = [ChannelText {
            name: "review".into(),
            doc: None,
            request_type: "str".into(),
            reply_type: "Verdict".into(),
            request: "{}".into(),
            reply: "{}".into(),
        }];
        let inputs = [InputBrief {
            name: "path".into(),
            type_name: "str".into(),
            preview: "\"src\"".into(),
        }];
        let told = |aliases: Option<&[&str]>| {
            let own = aliases.map(|aliases| ("Reviewer", aliases));
            preamble(&instance(&channels, own), &inputs)
        };
        let plain = told(Some(&[]));
        let at = plain
            .find("`self` is the `Reviewer` instance your parent's code constructed")
            .unwrap_or_else(|| panic!("{plain}"));
        let input = plain.find("- `path` (str)").expect("the input");
        let answer = plain.find("You answer requests").expect("the channels");
        assert!(input < at && at < answer, "{plain}");
        for said in [
            "the same object, not a copy",
            "its ordinary methods run here",
            "calling one on `self` raises `TypeError`",
            "a read followed by a write is not",
            "Keep your own state on `self`",
            "You run until you are released",
        ] {
            assert!(plain.contains(said), "{said:?} in {plain}");
        }
        assert!(!plain.contains("its constructor was given"), "{plain}");
        let one = told(Some(&["task"]));
        assert!(
            one.contains("`task` is the `task` its constructor was given, the same object."),
            "{one}"
        );
        let two = told(Some(&["task", "repo"]));
        assert!(
            two.contains(
                "`task` is the `task` its constructor was given, the same object. `repo` is the \
                 `repo` its constructor was given, the same object."
            ),
            "{two}"
        );
        let hosted = told(None);
        assert!(!hosted.contains("`self`"), "{hosted}");
    }

    fn channel(name: &str) -> ChannelText {
        ChannelText {
            name: name.into(),
            doc: Some("Review a path.".into()),
            request_type: "str".into(),
            reply_type: "Verdict".into(),
            request: "{}".into(),
            reply: "{}".into(),
        }
    }

    /// An instance's instructions are composed in the documented order, each part its own and none
    /// formatted into another: how it acts, what it is, its class's docstring, each section its
    /// class contributed under its title as returned -- braces and all -- the inputs, `self`, the
    /// facilities it was given, the channels, and what every agent is told last, which has the
    /// rule about a binding after them. A class that contributed none has no section.
    #[test]
    fn an_instance_is_composed_in_the_documented_order() {
        let channels = [channel("review")];
        let inputs = [InputBrief {
            name: "path".into(),
            type_name: "str".into(),
            preview: "\"src\"".into(),
        }];
        let sections = [("Task", "Review {x} and {}"), ("Rules", "Be brief.")];
        let files = [given(
            "files",
            FacilityKind::Binding,
            "git.Repo",
            "The repository.",
        )];
        let told = |sections: &[(&str, &str)]| {
            child_preamble(
                None,
                View::Given(&files),
                Window::DEFAULT,
                "Name {the} capital.",
                &inputs,
                &ChildBrief::Instance {
                    channels: &channels,
                    own: Some(("Reviewer", &[])),
                    sections,
                    root: false,
                    skills: &[],
                },
            )
        };
        let text = told(&sections);
        let positions: Vec<usize> = [
            ACT.trim_end(),
            "You are a child agent",
            "Name {the} capital.",
            "## Task\n\nReview {x} and {}\n\n## Rules\n\nBe brief.",
            "- `path` (str)",
            "`self` is the `Reviewer` instance",
            "You were given these to act through",
            "- `files`, a binding the host holds",
            "You answer requests",
            "`runtime.channels[\"review\"]`: Review a path.",
            "Your whole conversation is in",
            "`await asyncio.to_thread(lambda: files.f(x))`",
        ]
        .map(|needle| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle:?} in {text}"))
        })
        .to_vec();
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{positions:?} in {text}"
        );
        assert!(!told(&[]).contains("## "), "{}", told(&[]));
    }

    /// A root is told that the program that owns the session constructed it and that no user
    /// reaches it, not that an agent's code launched it, and that its `self` is the instance that
    /// program constructed, which the program asks through its request methods. One the owner
    /// constructed in its kernel is told the session's skills, without how the user's message
    /// names one, ahead of what every agent is told last; a host program's root has no `self` and
    /// no skills, and a child an agent made is told of neither.
    #[test]
    fn a_root_is_told_the_program_constructed_it() {
        let channels = [channel("review")];
        let skills = [SkillBrief {
            name: "review-diff".into(),
            description: "Review a diff.".into(),
            package: Some("outrig_skills.review_diff".into()),
        }];
        let told = |own: Option<(&str, &[&str])>, root: bool, skills: &[SkillBrief]| {
            preamble(
                &ChildBrief::Instance {
                    channels: &channels,
                    own,
                    sections: &[],
                    root,
                    skills,
                },
                &[],
            )
        };
        let owned = told(Some(("Reviewer", &["task"])), true, &skills);
        for said in [
            "You are a root agent: the program that owns this session constructed you; no user \
             reaches you, and that program is who you answer.",
            "`self` is the `Reviewer` instance that program constructed in this interpreter for you",
            "`task` is the `task` its constructor was given, the same object.",
            "You run until that program releases you, or as the session ends.",
            "These skills come with the session",
            "- `review-diff` (`outrig_skills.review_diff`): Review a diff.",
        ] {
            assert!(owned.contains(said), "{said:?} in {owned}");
        }
        for unsaid in [
            "another agent's code",
            "Your parent's code holds",
            "d.skill",
            "`/name`",
        ] {
            assert!(!owned.contains(unsaid), "{unsaid:?} in {owned}");
        }
        assert!(
            owned.find("These skills come with the session")
                < owned.find("Your whole conversation"),
            "{owned}"
        );

        let hosted = told(None, true, &[]);
        assert!(hosted.contains("You are a root agent"), "{hosted}");
        assert!(
            !hosted.contains("`self`") && !hosted.contains("These skills"),
            "{hosted}"
        );
        let child = told(Some(("Reviewer", &[])), false, &[]);
        assert!(
            !child.contains("These skills") && !child.contains("root agent"),
            "{child}"
        );
    }

    /// A root whose class declares no request channel is told so, and nothing of answering; it
    /// still has the wait rules, and its one round begins as it is launched.
    #[test]
    fn a_root_that_declares_no_channel_is_told_of_none() {
        let text = preamble(
            &ChildBrief::Instance {
                channels: &[],
                own: Some(("Reviewer", &[])),
                sections: &[],
                root: true,
                skills: &[],
            },
            &[],
        );
        for said in [
            "Your class declares no request channel, so no request arrives and you have no \
             `runtime.complete`.",
            "Ending your round is not waiting",
            "A round begins as you are launched, and its opening line",
        ] {
            assert!(text.contains(said), "{said:?} in {text}");
        }
        assert!(!text.contains("You answer requests"), "{text}");
    }

    /// A root's catalog is bounded as the primary's is: at most [`SKILLS_LISTED_MAX`] skills, then
    /// how many more there are.
    #[test]
    fn a_roots_catalog_is_bounded_as_the_primarys_is() {
        let many: Vec<SkillBrief> = (0..SKILLS_LISTED_MAX + 10)
            .map(|n| SkillBrief {
                name: format!("skill-{n}"),
                description: "Does a thing.".into(),
                package: None,
            })
            .collect();
        let text = preamble(
            &ChildBrief::Instance {
                channels: &[],
                own: None,
                sections: &[],
                root: true,
                skills: &many,
            },
            &[],
        );
        assert_eq!(text.matches("\n- `skill-").count(), SKILLS_LISTED_MAX);
        assert!(
            text.contains("10 more are not listed here; `outrig.skills.catalog()` has every one."),
            "{text}"
        );
    }

    /// The orienting round opens on its lead: that it is for getting ready; for a root whose
    /// instructions carry a task, that the task is in them and the round is where it begins; and,
    /// when a request already waits, what waits, by channel and count, after the lead -- never
    /// what it says.
    #[test]
    fn the_orienting_lead_adds_the_task_and_what_waits_after_it() {
        assert_eq!(orienting(false, ""), ORIENTING);
        let waiting = "1 request is waiting on runtime.channels[\"review\"]";
        assert_eq!(
            orienting(true, ""),
            format!("{ORIENTING}. {ORIENTING_TASK}")
        );
        assert_eq!(orienting(false, waiting), format!("{ORIENTING}. {waiting}"));
        let opened = opening(&orienting(true, waiting), 0);
        assert!(
            opened.starts_with("[outrig] you have just been launched"),
            "{opened}"
        );
        assert!(
            opened.find("where you begin it") < opened.find("1 request is waiting"),
            "{opened}"
        );
        assert!(
            opened.ends_with("runtime.channels[\"review\"]."),
            "{opened}"
        );
    }

    /// Every child's preamble states the rule: up to two more rounds, the last a warning, then the
    /// cancel, and that ending a round is not waiting. A request child is also told that only
    /// `d.reply` and `d.fail` answer, and that code left running keeps no request open; a work
    /// child, and a call's, how to fail what it cannot complete.
    #[test]
    fn every_preamble_states_the_rule() {
        let channels = [ChannelText {
            name: "fizz".into(),
            doc: None,
            request_type: "str".into(),
            reply_type: "Thing".into(),
            request: "{}".into(),
            reply: "{}".into(),
        }];
        let call = ChildBrief::Call {
            signature: "f() -> Fact",
            result_type: "Fact",
            schema: "{}",
        };
        for brief in [
            ChildBrief::Work,
            call,
            ChildBrief::Request(&channels),
            instance(&channels, None),
            instance(&channels, Some(("Reviewer", &[]))),
        ] {
            let text = preamble(&brief, &[]);
            assert!(
                text.contains("followed by up to two more, the last warning that")
                    && text.contains("cancelled if it ends the same way")
                    && text.contains("Ending your round is not waiting"),
                "{text}"
            );
            let request = matches!(brief, ChildBrief::Request(_) | ChildBrief::Instance { .. });
            assert_eq!(
                text.contains("what you write yourself is not delivered"),
                request,
                "{text}"
            );
            assert_eq!(text.contains("keeps no request open"), request, "{text}");
            assert_eq!(text.contains("never what it holds"), request, "{text}");
            assert_eq!(
                text.contains("`await runtime.fail(message)`"),
                !request,
                "{text}"
            );
            assert!(!text.contains("fails."), "{text}");
        }
    }
}
