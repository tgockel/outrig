//! Facilities end to end: an agent given a binding in its constructor's
//! `facilities=` dict sees that binding under the name it was given and no
//! other, its children made without a dict see the same, and every agent's
//! calls on the binding go over its own kernel's connections and are
//! attributed to it. On the host's interpreter, with the routed mock provider
//! and the fixture library bound twice, as `repo` and `notes`. The
//! interpreter's own half -- classification, plain objects, the mapping read as
//! it stands -- is `interpreter_tests`'.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use super::agent_tests::{messages, round};
use super::children_tests::{
    Tree, data, exec_outputs, kind, of_agent, primary_runs, printed, raised, serving, tree_built,
};
use super::instance_tests::{launching, subject};
use super::mock_http::{RecordedRequest, Responder, submit, system_text, text_reply};
use crate::harness::Service;
use crate::harness::event::Event;
use crate::harness::services_tests::Fixture;
use crate::python::testing::{Flag, pip_env, py, wheel_links, within};

/// The fixture library bound twice, where no approval is asked for.
const BINDINGS: &str = r#"
[bindings.repo]
description = "The fixture library."
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make"

[bindings.notes]
description = "The fixture's notes."
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make"
"#;

/// Where the fixture library is installed for these tests, kept between runs.
fn packages() -> PathBuf {
    std::env::temp_dir().join(format!(
        "outrig-test-facilities-packages-{}",
        nix::unistd::getuid()
    ))
}

/// A session holding [`BINDINGS`], its model calls answered by `routes`. The
/// second value is its approvals' directory, kept while the session runs.
async fn session(var: &str, routes: Vec<(&'static str, Responder)>) -> (Tree, tempfile::TempDir) {
    let approvals = tempfile::tempdir().expect("an approvals directory");
    let store = approvals.path().to_path_buf();
    // The wheels are built off this test's runtime, which their build cannot run inside.
    let wheels =
        std::thread::scope(|scope| scope.spawn(wheel_links).join()).expect("the wheels build");
    let tree = tree_built(var, BINDINGS, routes, None, move |builder| {
        builder.binding_places(&packages(), pip_env(wheels), &store)
    })
    .await;
    (tree, approvals)
}

/// An instance's rounds: its launch round runs `launch`, and each later one
/// `later`; the call after a result ends the round.
fn rounds(launch: &str, later: &str) -> Responder {
    let (launch, later) = (py(launch), py(later));
    Arc::new(move |request: &RecordedRequest| {
        let after_result = messages(request)
            .last()
            .and_then(|message| message["content"].as_array())
            .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"));
        let canned = if after_result {
            text_reply("ended")
        } else if launching(request) {
            submit("launch", &launch)
        } else {
            submit("later", &later)
        };
        Box::pin(async move { canned })
    })
}

/// The id of the agent an event's subject names.
fn agent_of(event: &Event) -> String {
    let subject = subject(event);
    subject
        .strip_prefix("agent/")
        .unwrap_or(subject)
        .to_string()
}

/// Each child an `agent.instance.started` of class `qualname` names, with the
/// facilities it carries, in order.
fn instances(events: &[Arc<Event>], qualname: &str) -> Vec<(String, Value)> {
    kind(events, "agent.instance.started", None)
        .into_iter()
        .filter(|event| data(event)["qualname"] == qualname)
        .map(|event| (agent_of(event), data(event)["facilities"].clone()))
        .collect()
}

/// The child the `agent.spawned` that `test` picks by its data and child made,
/// with the facilities it carries.
fn spawned(events: &[Arc<Event>], test: impl Fn(&Value, &str) -> bool) -> (String, Value) {
    kind(events, "agent.spawned", None)
        .into_iter()
        .map(|event| (agent_of(event), data(event)))
        .find(|(agent, data)| test(data, agent))
        .map(|(agent, data)| (agent, data["facilities"].clone()))
        .expect("the child's agent.spawned")
}

/// Whether `agent` made a call on binding `binding` whose arguments name
/// `label`.
fn called(events: &[Arc<Event>], agent: &str, binding: &str, label: &str) -> bool {
    kind(events, "hosted.received", None)
        .into_iter()
        .any(|event| {
            let d = data(event);
            d["agent"] == agent
                && d["binding"] == binding
                && d["operation"] == "call"
                && d["arguments"].to_string().contains(label)
        })
}

/// The primary's code: a reference from the primary, as an argument -- which
/// crosses as the binding's root, not the binding's `nested` -- and from
/// outside every execution; a sub-object refused as a facility; a `Reviewer` given `repo` as
/// `files`, asked once after it is ready; then the primary's own view, and
/// the children it makes without a mapping, or with an empty one.
const PRIMARY: &str = r#"
import _thread, threading, outrig
class Reviewer(outrig.Agent):
    """ROLE reviewer. Review with what you were given."""
    @outrig.agent
    async def review(self, path: str) -> str:
        """Review the file at `path`."""
class Plain(outrig.Agent):
    """ROLE plain. Say what you see."""
    @outrig.agent
    async def look(self, q: str) -> str:
        """Look at `q`."""
print(repo.ask('by-primary') > 0)
ref = outrig.facility('repo')
print(ref.ask('by-ref') > 0)
print(repo.takes(ref), repo.takes(repo.nested))
seen, done = [], threading.Event()
def outside():
    try:
        ref.ask('by-outside')
    except RuntimeError as e:
        seen.append('outside every execution' in str(e))
    done.set()
_thread.start_new_thread(outside, ())
done.wait(10)
print(seen)
try:
    Reviewer(facilities={'index': repo.list_records(1)})
except TypeError as e:
    print("'index'" in str(e))
r = Reviewer(facilities={'files': repo})
print(type(r.facilities['files']).__name__)
await r.ready()
print(await r.review('x'))
await r.release()
print(sorted(runtime.bindings), type(repo).__name__, sorted(outrig.bindings),
      runtime._kernel.facilities)
plain, empty = Plain(), Plain(facilities={})
await plain.ready()
await empty.ready()
await plain.release()
await empty.release()
w = await runtime.spawn('w', prompt='ROLE wide. Report what you see.')
print(await w.submit({}, result=str))
await runtime.release([w])
"#;

/// The reviewer's launch round: its view, a call of its own, two helpers --
/// one given the reviewer's own reference and a plain object, one given
/// nothing -- a work child made with no mapping, and a sub-object put in its
/// mapping, which the next read refuses; then an entry added for good.
const REVIEWER: &str = r#"
import __main__, outrig
me = runtime._kernel
print(type(files).__name__, [(b.name, b.kind) for b in runtime.bindings.values()])
try:
    outrig.bindings['repo']
except KeyError:
    print('no repo')
print(files.ask('by-reviewer') > 0,
      me._hosted['repo'] is not __main__._kernels['primary']._hosted['repo'])
class Tally:
    """A count kept here."""
class Helper(outrig.Agent):
    """ROLE helper. Help with what you were given."""
    @outrig.agent
    async def help(self, q: str) -> str:
        """Help with `q`."""
given = Helper(facilities={'files': me.facilities['files'], 'tally': Tally()})
inherited = Helper()
await given.ready()
await inherited.ready()
await given.release()
await inherited.release()
s = await runtime.spawn('s', prompt='ROLE spawned. Report what you see.')
print(await s.submit({}, result=str))
await runtime.release([s])
me.facilities['rows'] = files.list_records(1)
try:
    runtime.bindings
except TypeError as e:
    print("'rows'" in str(e))
del me.facilities['rows']
me.facilities['extra'] = 5
"#;

/// The reviewer's later round: one review, answered with its view as it now
/// stands.
const REVIEW: &str = r#"
d = await runtime.channels['review'].receive()
await d.reply(f"{sorted(runtime.bindings)} {'extra' in globals()}")
"#;

/// A helper's launch round: its view, and a call of its own.
const HELPER: &str = r#"
print(type(files).__name__, sorted(runtime.bindings), 'repo' in globals(), 'notes' in globals(),
      files.ask(f'by-{runtime._kernel.agent}') > 0)
"#;

/// The reviewer's work child's round: its view, and a call of its own.
const SPAWNED: &str = r#"
await runtime.complete(f"{type(files).__name__} {sorted(runtime.bindings)} "
                       f"{'repo' in globals()} {files.ask('by-spawned') > 0}")
"#;

/// A `Plain`'s launch round: its view.
const PLAIN: &str = "print(sorted(runtime.bindings), 'repo' in globals(), 'notes' in globals())";

/// The primary's work child's round: its view.
const WIDE: &str = "await runtime.complete(f\"{sorted(runtime.bindings)} {'repo' in globals()}\")";

/// An agent given `{"files": repo}` sees `files` and no other binding, in its
/// instructions, `runtime.bindings`, its globals and `outrig.bindings`, while
/// its parent's view stays the session's; the children it makes without a
/// mapping see the same, and one it gives a plain object sees that too. Each
/// agent's calls on the binding are its own: the primary's, the reviewer's,
/// each helper's and the work child's, three levels down and one binding. A
/// sub-object is refused as a facility, at construction and at a read; a
/// reference outside every execution raises; an entry added later is listed
/// but is no global, and the instructions stay as composed. The primary's
/// children made without a mapping see every binding, and one given an empty
/// mapping sees none. The spawns carry each view.
#[tokio::test]
async fn an_agent_sees_what_it_was_given_and_its_calls_are_its_own() {
    let (mut tree, _approvals) = session(
        "OUTRIG_TEST_FACILITIES_KEY",
        vec![
            ("ROLE reviewer", rounds(REVIEWER, REVIEW)),
            ("ROLE helper", serving(HELPER)),
            ("ROLE spawned", serving(SPAWNED)),
            ("ROLE plain", serving(PLAIN)),
            ("ROLE wide", serving(WIDE)),
            ("", primary_runs(PRIMARY)),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let (_, events, requests) = tree.end().await;

    assert_eq!(
        printed(&requests, "p1"),
        "True\nTrue\nFalse True\n[True]\nTrue\nFacility\n['extra', 'files'] False\n\
         ['notes', 'repo'] HostedRoot ['notes', 'repo'] None\n['notes', 'repo'] True\n"
    );

    // The reviewer's view, and its spawn's.
    let reviewers = instances(&events, "Reviewer");
    assert_eq!(reviewers.len(), 1, "{reviewers:?}");
    let (reviewer, reviewer_given) = reviewers.into_iter().next().expect("one Reviewer");
    let files = |target_type: &Value| json!({"name": "files", "kind": "binding", "target": "repo", "type": target_type});
    let repo_type = &reviewer_given[0]["type"];
    assert!(
        repo_type.as_str().is_some_and(|t| !t.is_empty()),
        "{reviewer_given}"
    );
    assert_eq!(reviewer_given, json!([files(repo_type)]));
    assert_eq!(
        spawned(&events, |_, agent| agent == reviewer).1,
        reviewer_given
    );
    let launch = exec_outputs(&events, &format!("agent/{reviewer}"));
    assert_eq!(
        launch[0],
        "HostedRoot [('files', 'binding')]\nno repo\nTrue True\n\
         HostedRoot ['files'] False True\nTrue\n"
    );

    // Its helpers: one given its reference and a plain object, one inheriting its mapping.
    let helpers = instances(&events, "Helper");
    assert_eq!(helpers.len(), 2, "{helpers:?}");
    let (given, given_facilities) = &helpers[0];
    assert_eq!(given_facilities[0], files(repo_type));
    assert_eq!(
        (&given_facilities[1]["name"], &given_facilities[1]["kind"]),
        (&json!("tally"), &json!("object"))
    );
    assert!(given_facilities[1]["target"].is_null());
    assert!(
        given_facilities[1]["type"]
            .as_str()
            .is_some_and(|t| t.ends_with(".Tally")),
        "{given_facilities}"
    );
    let (inherited, inherited_facilities) = &helpers[1];
    assert_eq!(*inherited_facilities, json!([files(repo_type)]));
    assert_eq!(
        exec_outputs(&events, &format!("agent/{given}")),
        ["HostedRoot ['files', 'tally'] False False True\n"]
    );
    assert_eq!(
        exec_outputs(&events, &format!("agent/{inherited}")),
        ["HostedRoot ['files'] False False True\n"]
    );
    let (work, work_facilities) = spawned(&events, |data, _| data["name"] == "s");
    assert_eq!(work_facilities, json!([files(repo_type)]));

    // Every caller's own: one binding, its calls from three levels.
    for (agent, label) in [
        ("primary", "by-primary"),
        ("primary", "by-ref"),
        (reviewer.as_str(), "by-reviewer"),
        (given.as_str(), &format!("by-{given}")),
        (inherited.as_str(), &format!("by-{inherited}")),
        (work.as_str(), "by-spawned"),
    ] {
        assert!(
            called(&events, agent, "repo", label),
            "{agent} called repo with {label}"
        );
    }
    assert!(
        !kind(&events, "hosted.received", None)
            .iter()
            .any(|event| data(event)["arguments"].to_string().contains("by-outside")),
        "nothing outside every execution was sent"
    );

    // What each was told: its facilities, and no other binding.
    let reviewer_requests = of_agent(&requests, "ROLE reviewer");
    let told = system_text(&reviewer_requests[0].body);
    assert!(
        told.contains("- `files`, a binding the host holds: The fixture library."),
        "{told}"
    );
    assert!(
        !told.contains("- `repo`") && !told.contains("`notes`"),
        "{told}"
    );
    assert!(
        reviewer_requests.len() > 2
            && reviewer_requests
                .iter()
                .all(|request| system_text(&request.body) == told),
        "composed once, the later round's the launch round's"
    );
    assert_eq!(
        kind(
            &events,
            "model.instructions",
            Some(&format!("agent/{reviewer}"))
        )
        .len(),
        1
    );
    let helper_told: Vec<String> = of_agent(&requests, "ROLE helper")
        .iter()
        .map(|request| system_text(&request.body))
        .collect();
    assert!(
        helper_told
            .iter()
            .any(|told| told.contains("in this interpreter: A count kept here.")),
        "{helper_told:?}"
    );
    assert!(
        helper_told
            .iter()
            .all(|told| told.contains("- `files`, a binding") && !told.contains("`notes`")),
        "{helper_told:?}"
    );
    let work_told = system_text(&of_agent(&requests, "ROLE spawned")[0].body);
    assert!(
        work_told.contains("- `files`, a binding") && !work_told.contains("`notes`"),
        "{work_told}"
    );

    // The primary's children: the session's view without a mapping, nothing with an empty one.
    let plains = instances(&events, "Plain");
    assert_eq!(
        plains.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>(),
        [Value::Null, json!([])]
    );
    assert_eq!(
        exec_outputs(&events, &format!("agent/{}", plains[0].0)),
        ["['notes', 'repo'] True True\n"]
    );
    assert_eq!(
        exec_outputs(&events, &format!("agent/{}", plains[1].0)),
        ["[] False False\n"]
    );
    let plain_told: Vec<String> = of_agent(&requests, "ROLE plain")
        .iter()
        .map(|request| system_text(&request.body))
        .collect();
    assert!(
        plain_told
            .iter()
            .any(|told| told.contains("- `repo`: The fixture library.")
                && told.contains("- `notes`: The fixture's notes.")),
        "{plain_told:?}"
    );
    assert!(
        plain_told.iter().any(|told| !told.contains("- `repo`")),
        "{plain_told:?}"
    );
    let (_, wide) = spawned(&events, |data, _| data["name"] == "w");
    assert!(wide.is_null(), "{wide}");
    assert_eq!(
        kind(&events, "agent.spawned", None).len(),
        7,
        "the refused Reviewer launched nothing"
    );
}

/// The primary's code for a service given as a facility: its own call, a `Helper` given the
/// service by reference and asked once, its own call after the helper's release, and a `Bare`
/// given nothing.
const SERVED: &str = r#"
import outrig
class Helper(outrig.Agent):
    """ROLE helper. Use what you were given."""
    @outrig.agent
    async def go(self, q: str) -> str:
        """Do `q`."""
class Bare(outrig.Agent):
    """ROLE bare. Say what you see."""
    @outrig.agent
    async def look(self, q: str) -> str:
        """Look at `q`."""
print(svc.echo(x=1))
h = Helper(facilities={'svc': outrig.facility('svc')})
await h.ready()
print(await h.go('x'))
await h.release()
print(svc.echo(x=2))
b = Bare(facilities={})
await b.ready()
await b.release()
"#;

/// The helper's launch round: its view, and a call of its own.
const SERVED_HELPER: &str = r#"
print(svc.echo(x=3), type(svc).__qualname__, [(b.name, b.kind) for b in runtime.bindings.values()])
"#;

/// The helper's request: answered with another call of its own.
const SERVED_GO: &str = r#"
d = await runtime.channels['go'].receive()
await d.reply(str(svc.echo(x=4)))
"#;

/// The bare child's launch round: what it can find of the service, which is nothing.
const SERVED_BARE: &str = r#"
import outrig
try:
    outrig.bindings['svc']
except KeyError:
    print('no svc')
try:
    svc
except NameError:
    print('NameError')
print(list(runtime.bindings))
"#;

/// A service in a facility dict: the child given it has an object of its own, whose calls are
/// attributed to the child, and the parent's go on after the child's release; a child given
/// nothing finds no service anywhere, nor in its instructions. The primary is told of the
/// service beside its bindings, and the child of the one it was given.
#[tokio::test]
async fn a_service_given_as_a_facility_is_the_childs_own() {
    let fixture = Fixture::new();
    let given = Arc::clone(&fixture) as Arc<dyn Service>;
    let mut tree = tree_built(
        "OUTRIG_TEST_SERVICE_FACILITIES_KEY",
        "",
        vec![
            ("ROLE helper", rounds(SERVED_HELPER, SERVED_GO)),
            ("ROLE bare", serving(SERVED_BARE)),
            ("", primary_runs(SERVED)),
        ],
        None,
        move |builder| builder.serve("svc", given),
    )
    .await;
    round(&mut tree.session, "go").await;
    let (report, events, requests) = tree.end().await;
    assert!(report.services.is_empty(), "{report:?}");
    assert_eq!(printed(&requests, "p1"), "{'x': 1}\n{'x': 4}\n{'x': 2}\n");

    let helpers = instances(&events, "Helper");
    assert_eq!(helpers.len(), 1, "{helpers:?}");
    let (helper, facilities) = &helpers[0];
    assert_eq!(
        *facilities,
        json!([{"name": "svc", "kind": "service", "target": "svc",
                "type": "outrig.services.svc"}])
    );
    assert_eq!(
        exec_outputs(&events, &format!("agent/{helper}"))[0],
        "{'x': 3} svc [('svc', 'service')]\n"
    );
    let callers: Vec<(i64, String)> = fixture
        .seen()
        .into_iter()
        .map(|(_, args, agent)| (args["x"].as_i64().unwrap_or_default(), agent))
        .collect();
    assert_eq!(
        callers,
        [
            (1, "primary".to_string()),
            (3, helper.clone()),
            (4, helper.clone()),
            (2, "primary".to_string()),
        ]
    );
    let received: Vec<String> = kind(&events, "service.call.received", None)
        .iter()
        .map(|event| {
            data(event)["agent"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(
        received,
        ["primary", helper.as_str(), helper.as_str(), "primary"]
    );

    let bares = instances(&events, "Bare");
    assert_eq!(
        exec_outputs(&events, &format!("agent/{}", bares[0].0)),
        ["no svc\nNameError\n[]\n"]
    );
    let bare_told = system_text(&of_agent(&requests, "ROLE bare")[0].body);
    assert!(!bare_told.contains("svc"), "{bare_told}");
    let helper_told = system_text(&of_agent(&requests, "ROLE helper")[0].body);
    for said in [
        "- `svc`, a service the host runs: The test service.",
        "hands back a ticket: wait for its completion with `runtime.wait`",
    ] {
        assert!(helper_told.contains(said), "{said}: {helper_told}");
    }
    let primary_told = system_text(&requests[0].body);
    for said in [
        "These names are services the host runs",
        "- `svc`: The test service.",
        "Every call of a service's method is a request to the host",
    ] {
        assert!(primary_told.contains(said), "{said}: {primary_told}");
    }
}

/// A method that starts long work hands back a ticket within the round, and the agent waits
/// for the completion the owner sends on the user channel with `runtime.wait`, no call held
/// meanwhile.
#[tokio::test]
async fn a_ticket_is_waited_for_on_a_channel() {
    let fixture = Fixture::new();
    let given = Arc::clone(&fixture) as Arc<dyn Service>;
    let waiting = Flag::new();
    let source = format!(
        r#"
await runtime.channels['user'].receive()
print(svc.ask(question='how many?'))
forever = asyncio.create_task(asyncio.Event().wait())
open({flag}, 'w').close()
try:
    await runtime.wait({{forever}}, timeout=30)
except runtime.MessageAvailable:
    print('answered', (await runtime.channels['user'].receive()).body)
"#,
        flag = waiting.py()
    );
    let mut tree = tree_built(
        "OUTRIG_TEST_SERVICE_TICKETS_KEY",
        "",
        vec![("", primary_runs(&source))],
        None,
        move |builder| builder.serve("svc", given),
    )
    .await;
    let user = tree.session.user_channel().expect("a primary agent");
    let gathered = Arc::clone(&tree.events);
    let meanwhile = async {
        raised(&waiting).await;
        let events = gathered.lock().expect("events").clone();
        assert_eq!(
            (
                kind(&events, "service.call.received", None).len(),
                kind(&events, "service.call.ended", None).len()
            ),
            (1, 1),
            "the ticket's call ended before the wait"
        );
        user.send("t-1 says 42").await.expect("delivered");
    };
    let ((), ()) = tokio::join!(
        async {
            round(&mut tree.session, "go").await;
        },
        within(meanwhile)
    );
    let (_, _, requests) = tree.end().await;
    assert_eq!(
        printed(&requests, "p1"),
        "{'ticket': 't-1'}\nanswered t-1 says 42\n"
    );
    assert_eq!(fixture.calls(), 1);
}
