//! The session's agents as its owner reads them: each case runs a root its owner constructed
//! against the routed mock provider, its code launching children, and reads
//! [`Session::roster`]. The cases about what a child is doing and holds run three times -- with
//! every event gathered, with no subscription, and with a subscription of one event that is never
//! read -- and read the same each time: the roster is the session's registry of its agents, never
//! the stream.

use std::time::Duration;

use serde_json::json;

use super::agent_tests::round;
use super::children_tests::{
    UNTIL, Watch, child_runs, data, kind, primary_waits, serving, serving_each, tree,
};
use super::instance_tests::{fizzing, ready_then, subject};
use super::mock_http::{failure, scripted, text_reply};
use super::root_tests::{agent_of, owned_watched, reviewer};
use super::runner;
use super::stop_tests::ready_root_with;
use crate::harness::event::{Received, RequestEnd};
use crate::harness::{
    AgentKind, AgentRow, AgentState, ConstructSpec, Parent, RootError, RootHandle, Roster,
    RosterState, Session, SessionBuilder,
};
use crate::python::testing::{Flag, py, within};

/// A root's round that takes the request it opened on, runs `source` with `until(test)`
/// defined, and answers the request.
fn reviewing(source: &str) -> String {
    format!(
        "{UNTIL}\nd = await runtime.channels['review'].receive()\n{}\n\
         await d.reply(Verdict(path=d.body, ok=True))",
        py(source)
    )
}

/// Python that launches request child `name`, marked `ROLE <role>`, with one channel, `ask`.
fn asking(name: &str, role: &str) -> String {
    format!(
        "{name} = await runtime.spawn('{name}', prompt='ROLE {role}', requests={{'ask': (str, str)}})"
    )
}

/// Python that launches a work child `w`, gives it a submission, waits until it works on it,
/// and queues two more behind it.
const WORKS: &str = "w = await runtime.spawn('w', prompt='ROLE worker')\n\
                     s1 = w.submit({}, result=str)\n\
                     await until(lambda: s1.status.state == 'running')\n\
                     s2 = w.submit({}, result=str)\n\
                     s3 = w.submit({}, result=str)";

/// Ask `root` to review `path`, and wait for its answer: the round has run its code.
async fn review(root: &RootHandle, path: &str) {
    let request = root.request("review", json!(path)).expect("sent");
    within(request.outcome()).await.expect("answered");
}

/// The session's roster once `holds` does of it, read every 10 ms.
async fn roster_until(session: &Session, holds: impl Fn(&Roster) -> bool) -> Roster {
    within(async {
        loop {
            let roster = session.roster();
            if holds(&roster) {
                return roster;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// The row of the agent its launch named `name`.
fn named<'a>(roster: &'a Roster, name: &str) -> Option<&'a AgentRow> {
    roster
        .agents
        .iter()
        .find(|row| row.name.as_deref() == Some(name))
}

/// Each row's name, in order.
fn names(roster: &Roster) -> Vec<Option<&str>> {
    roster
        .agents
        .iter()
        .map(|row| row.name.as_deref())
        .collect()
}

/// Each row's id, in order.
fn ids(roster: &Roster) -> Vec<&str> {
    roster.agents.iter().map(|row| row.id.as_str()).collect()
}

/// What a child's row holds: unread, queued, taken.
fn held(row: &AgentRow) -> (usize, usize, usize) {
    let held = row.outstanding.expect("a child holds requests");
    (held.unread, held.queued, held.taken)
}

/// Whether the row named `name` holds `counts`.
fn holds(roster: &Roster, name: &str, counts: (usize, usize, usize)) -> bool {
    named(roster, name).is_some_and(|row| held(row) == counts)
}

/// A root, and two request children it launched -- one whose model calls fail with a 500, which
/// its runner will try again -- are three rows: the root names its class, the owner as its
/// parent and depth 1; the failing child is `unavailable`, its next attempt the live first wait
/// after the failure, holding the request its round opened on unread. Once the root releases it,
/// its kernel is gone and so is its row.
#[tokio::test]
async fn a_root_and_its_children_are_listed_one_retrying() {
    for watch in Watch::ALL {
        retrying(watch).await;
    }
}

async fn retrying(watch: Watch) {
    let (tree, _app, root) = ready_root_with(
        "OUTRIG_TEST_ROSTER_RETRYING_KEY",
        watch,
        vec![
            ("ROLE flaky", scripted(vec![failure(500)])),
            ("ROLE quiet", scripted(vec![text_reply("quiet")])),
        ],
        serving_each(vec![
            reviewing(&format!(
                "{}\nh = a.channels['ask'].request('x')\n{}",
                asking("a", "flaky"),
                asking("b", "quiet")
            )),
            reviewing("await runtime.release([a])"),
        ]),
    )
    .await;
    // The live first wait after a failed round, which the next runner to start -- `a`'s, launched
    // before `b` -- waits.
    runner::FIRST_WAIT.set(Some(Duration::from_secs(2)));
    review(&root, "a.py").await;
    runner::FIRST_WAIT.set(None);
    let roster = roster_until(&tree.session, |roster| {
        named(roster, "a").is_some_and(|a| {
            matches!(
                a.state,
                Some(RosterState::Agent(AgentState::Unavailable { .. }))
            )
        }) && named(roster, "Reviewer").is_some_and(|root| {
            matches!(
                root.state,
                Some(RosterState::Agent(AgentState::Idle { .. }))
            )
        })
    })
    .await;
    let id = agent_of(&root);
    assert_eq!(
        names(&roster),
        [Some("Reviewer"), Some("a"), Some("b")],
        "{watch:?}"
    );
    let top = &roster.agents[0];
    assert_eq!(top.id, id);
    assert_eq!(
        top.kind,
        AgentKind::Root {
            class: "review.agents.Reviewer".to_string()
        }
    );
    assert_eq!(
        (&top.parent, top.depth, top.model.as_str()),
        (&Some(Parent::Owner), 1, "fast")
    );
    assert_eq!(held(top), (0, 0, 0));
    let a = named(&roster, "a").expect("listed");
    assert_eq!(a.kind, AgentKind::Request { class: None });
    assert_eq!(
        (&a.parent, a.depth, a.model.as_str()),
        (&Some(Parent::Agent(id.clone())), 2, "fast")
    );
    let Some(RosterState::Agent(
        unavailable @ AgentState::Unavailable {
            class,
            since,
            next_attempt,
            ..
        },
    )) = &a.state
    else {
        panic!("{watch:?}: {a:?}");
    };
    assert!(class.starts_with("HTTP 500"), "{class}");
    let wait = next_attempt.duration_since(*since).expect("later");
    assert!(
        wait >= Duration::from_secs(2) && wait <= Duration::from_secs(16),
        "{watch:?}: {wait:?}"
    );
    assert_eq!(a.last_stop.as_ref(), Some(unavailable));
    assert_eq!(held(a), (1, 0, 0), "the request its round opened on");
    let b = named(&roster, "b").expect("listed");
    assert_eq!(
        b.state,
        Some(RosterState::Agent(AgentState::Launching)),
        "no round of it has opened"
    );
    review(&root, "b.py").await;
    assert_eq!(
        names(&tree.session.roster()),
        [Some("Reviewer"), Some("b")],
        "{watch:?}"
    );
    tree.end().await;
}

/// A child whose code never yields, released, reads `releasing` while its release waits on its
/// kernel and `wedged` once that wait is over, and stays listed: the roster's children are what
/// `children-max` counts. Once its kernel ends, its row is gone.
#[tokio::test]
async fn a_wedged_child_stays_listed_until_its_kernel_ends() {
    for watch in Watch::ALL {
        wedged(watch).await;
    }
}

async fn wedged(watch: Watch) {
    let flag = Flag::new();
    let stuck = format!(
        "d = await runtime.channels['ask'].receive()\n{}",
        flag.blocked()
    );
    let (tree, _app, root) = ready_root_with(
        "OUTRIG_TEST_ROSTER_WEDGED_KEY",
        watch,
        vec![("ROLE stuck", scripted(child_runs("s1", &stuck)))],
        serving_each(vec![
            reviewing(&format!(
                "{}\nh = s.channels['ask'].request('x')\n\
                 await until(lambda: h.status.state == 'received')",
                asking("s", "stuck")
            )),
            reviewing("await runtime.release([s])"),
        ]),
    )
    .await;
    review(&root, "a.py").await;
    let releasing = root.request("review", json!("b.py")).expect("sent");
    roster_until(&tree.session, |roster| {
        named(roster, "s").is_some_and(|s| s.state == Some(RosterState::Releasing))
    })
    .await;
    within(releasing.outcome()).await.expect("answered");
    let roster = tree.session.roster();
    let s = named(&roster, "s").expect("a wedged child is listed");
    assert_eq!(s.state, Some(RosterState::Wedged), "{watch:?}");
    assert_eq!(s.last_stop, Some(AgentState::Released));
    let children = roster
        .agents
        .iter()
        .filter(|row| row.parent.is_some())
        .count();
    assert_eq!(children, 2, "{watch:?}: {roster:?}");
    assert_eq!(children, tree.session.children().resident());
    flag.raise();
    roster_until(&tree.session, |roster| named(roster, "s").is_none()).await;
    tree.end().await;
}

/// What each child holds is every request made of it that has not ended: a request child that
/// took one of two holds one unread and one taken, then, once it answers, the unread one alone,
/// and its row is gone once it is released. A work child working on one submission, two more
/// waiting behind it, holds the two queued and the one taken.
#[tokio::test]
async fn a_childs_row_holds_every_request_not_ended() {
    for watch in Watch::ALL {
        outstanding(watch).await;
    }
}

async fn outstanding(watch: Watch) {
    let (go, done) = (Flag::new(), Flag::new());
    let holder = format!(
        "d1 = await runtime.channels['ask'].receive()\n{}\nawait d1.reply('one')\n{}",
        go.awaited(),
        done.awaited(),
    );
    let (tree, _app, root) = ready_root_with(
        "OUTRIG_TEST_ROSTER_OUTSTANDING_KEY",
        watch,
        vec![
            ("ROLE holder", scripted(child_runs("k1", &holder))),
            ("ROLE worker", scripted(child_runs("w1", &done.awaited()))),
        ],
        serving_each(vec![
            reviewing(&format!(
                "{}\nh1 = k.channels['ask'].request('first')\n\
                 h2 = k.channels['ask'].request('second')\n{WORKS}",
                asking("k", "holder")
            )),
            reviewing("await runtime.release([k])"),
        ]),
    )
    .await;
    review(&root, "a.py").await;
    let roster = roster_until(&tree.session, |roster| {
        holds(roster, "k", (1, 0, 1)) && holds(roster, "w", (0, 2, 1))
    })
    .await;
    assert_eq!(
        named(&roster, "k").expect("listed").kind,
        AgentKind::Request { class: None }
    );
    assert_eq!(named(&roster, "w").expect("listed").kind, AgentKind::Work);
    go.raise();
    roster_until(&tree.session, |roster| holds(roster, "k", (1, 0, 0))).await;
    review(&root, "b.py").await;
    let roster = tree.session.roster();
    assert!(named(&roster, "k").is_none(), "{watch:?}: {roster:?}");
    assert_eq!(held(named(&roster, "w").expect("listed")), (0, 2, 1));
    done.raise();
    tree.end().await;
}

/// The roster read as soon as `agent.spawned` is heard on a subscription lists the agent: the
/// registry holds it before the event is published.
#[tokio::test]
async fn an_agent_is_listed_once_its_spawn_is_heard() {
    let mut subscription = None;
    let (tree, _app) = owned_watched(
        "OUTRIG_TEST_ROSTER_SPAWNED_KEY",
        "",
        vec![
            ("ROLE quiet", scripted(vec![text_reply("quiet")])),
            (
                "ROLE reviewer",
                ready_then(serving(&reviewing(&asking("b", "quiet")))),
            ),
        ],
        Watch::Unwatched,
        |mut builder: SessionBuilder| {
            subscription = Some(builder.subscribe());
            builder
        },
    )
    .await;
    let mut subscription = subscription.expect("subscribed");
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    let request = root.request("review", json!("a.py")).expect("sent");
    let mut spawned = Vec::new();
    while spawned.len() < 2 {
        let event = match within(subscription.recv()).await {
            Some(Received::Event(event)) => event,
            other => panic!("{other:?}"),
        };
        if event.kind() != "agent.spawned" {
            continue;
        }
        let roster = tree.session.roster();
        let agent = subject(&event)
            .strip_prefix("agent/")
            .expect("an agent's")
            .to_string();
        assert!(
            roster.agents.iter().any(|row| row.id == agent),
            "{agent}: {roster:?}"
        );
        spawned.push(agent);
    }
    assert_eq!(spawned, [agent_of(&root), "c2".to_string()]);
    within(request.outcome()).await.expect("answered");
    tree.end().await;
}

/// Taken once admission has closed, the roster lists outstanding exactly what the shutdown
/// report lists: a work child's taken submission and the two queued behind it, and a request
/// child's unread request, each ended `closed`, and nothing else. The close released each
/// child, whose kernel the session's end ends, so each reads `releasing`.
#[tokio::test]
async fn the_roster_agrees_with_the_report() {
    for watch in Watch::ALL {
        agrees(watch).await;
    }
}

async fn agrees(watch: Watch) {
    let done = Flag::new();
    let (tree, _app, root) = ready_root_with(
        "OUTRIG_TEST_ROSTER_AGREES_KEY",
        watch,
        vec![
            ("ROLE worker", scripted(child_runs("w1", &done.awaited()))),
            ("ROLE ignoring", scripted(child_runs("i1", &done.awaited()))),
        ],
        serving(&reviewing(&format!(
            "{WORKS}\n{}\nh = k.channels['ask'].request('x')",
            asking("k", "ignoring")
        ))),
    )
    .await;
    review(&root, "a.py").await;
    roster_until(&tree.session, |roster| {
        holds(roster, "w", (0, 2, 1)) && holds(roster, "k", (1, 0, 0))
    })
    .await;
    tree.session.close_admission();
    let roster = tree.session.roster();
    done.raise();
    let (report, _, _) = tree.end().await;
    assert!(
        roster
            .agents
            .iter()
            .all(|row| row.state == Some(RosterState::Releasing)),
        "{watch:?}: {roster:?}"
    );
    let mut listed = 0;
    for row in &roster.agents {
        let (unread, queued, taken) = held(row);
        let ended: Vec<RequestEnd> = report
            .requests
            .iter()
            .filter(|ended| ended.child == row.id)
            .map(|ended| ended.outcome)
            .collect();
        assert_eq!(ended.len(), unread + queued + taken, "{watch:?}: {row:?}");
        assert!(ended.iter().all(|outcome| *outcome == RequestEnd::Closed));
        listed += ended.len();
    }
    assert_eq!(listed, 4);
    assert_eq!(report.requests.len(), listed, "{:?}", report.requests);
}

/// A root whose constructor launched an agent and then raised is gone, and so is its node; the
/// agent it launched, wedged as the failure released it -- its orienting round's code never
/// yields -- still counts against `children-max`, and is listed, naming the root no row lists,
/// until its kernel ends.
#[tokio::test]
async fn a_failed_constructions_wedged_child_is_listed() {
    let flag = Flag::new();
    let (tree, _app) = owned_watched(
        "OUTRIG_TEST_ROSTER_ORPHAN_KEY",
        "",
        vec![("ROLE nested", scripted(child_runs("n1", &flag.blocked())))],
        Watch::Unwatched,
        |builder| builder,
    )
    .await;
    let failed = within(
        tree.session
            .construct(ConstructSpec::new("review.agents:Nesting", "sonnet")),
    )
    .await
    .expect("asked");
    assert!(
        matches!(
            within(failed.ready()).await,
            Err(RootError::Construct { .. })
        ),
        "the constructor raised"
    );
    let roster = tree.session.roster();
    let [nested] = &roster.agents[..] else {
        panic!("the nested agent alone: {roster:?}");
    };
    assert_eq!(nested.state, Some(RosterState::Wedged));
    assert_eq!(
        nested.kind,
        AgentKind::Request {
            class: Some("review.agents.Nested".to_string())
        }
    );
    let Some(Parent::Agent(parent)) = &nested.parent else {
        panic!("{nested:?}");
    };
    assert_ne!(parent, &nested.id);
    assert_eq!(nested.depth, 2);
    assert_eq!(roster.agents.len(), tree.session.children().resident());
    flag.raise();
    roster_until(&tree.session, |roster| roster.agents.is_empty()).await;
    tree.end().await;
}

/// A session with no primary agent lists nothing until its owner constructs a root, and nothing
/// again once the root is released.
#[tokio::test]
async fn a_session_with_no_root_lists_nothing() {
    let (tree, _app) = owned_watched(
        "OUTRIG_TEST_ROSTER_EMPTY_KEY",
        "",
        vec![(
            "ROLE reviewer",
            ready_then(scripted(vec![text_reply("nothing")])),
        )],
        Watch::Unwatched,
        |builder| builder,
    )
    .await;
    assert_eq!(tree.session.roster().agents, []);
    let root = within(tree.session.construct(reviewer()))
        .await
        .expect("constructed");
    within(root.ready()).await.expect("ready");
    assert_eq!(ids(&tree.session.roster()), [agent_of(&root)]);
    within(root.release()).await;
    assert_eq!(tree.session.roster().agents, []);
    tree.end().await;
}

/// A session with a primary agent lists it first, at depth 1 with no parent and no state -- its
/// owner drives its rounds -- and below it what its code launched: a work child, and an agent
/// class instance's child, which names its class.
#[tokio::test]
async fn a_primary_session_lists_its_primary_first() {
    let mut tree = tree(
        "OUTRIG_TEST_ROSTER_PRIMARY_KEY",
        "",
        vec![
            (
                "ROLE fizzer",
                ready_then(scripted(vec![text_reply("idle")])),
            ),
            (
                "",
                primary_waits(&format!(
                    "{}\nw = await runtime.spawn('w', prompt='ROLE worker')\n\
                     f = Fizzer()\nawait f.ready()",
                    fizzing("Fizzer", "fizzer")
                )),
            ),
        ],
    )
    .await;
    round(&mut tree.session, "go").await;
    let roster = tree.session.roster();
    let (_, events, _) = tree.end().await;
    assert_eq!(ids(&roster), ["primary", "c1", "c2"], "{roster:?}");
    let primary = &roster.agents[0];
    assert_eq!(
        (
            &primary.kind,
            &primary.parent,
            primary.depth,
            primary.model.as_str(),
            &primary.state
        ),
        (&AgentKind::Primary, &None, 1, "sonnet", &None)
    );
    let work = &roster.agents[1];
    assert_eq!(
        (&work.kind, &work.parent, work.depth, work.name.as_deref()),
        (
            &AgentKind::Work,
            &Some(Parent::Agent("primary".to_string())),
            2,
            Some("w")
        )
    );
    let started = data(kind(&events, "agent.instance.started", Some("agent/c2"))[0]);
    let class = format!(
        "{}.{}",
        started["module"].as_str().expect("a module"),
        started["qualname"].as_str().expect("a qualname")
    );
    assert!(class.ends_with(".Fizzer"), "{class}");
    assert_eq!(
        roster.agents[2].kind,
        AgentKind::Request { class: Some(class) }
    );
}
