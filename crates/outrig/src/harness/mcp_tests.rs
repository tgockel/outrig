//! MCP services on the host: declarations placed by fake placements -- rmcp servers in memory,
//! no container -- started, classified, bound into the service table and called through the
//! generic object, quiesced and closed.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::bindings::Brief;
use super::bindings_tests::{output, start, stop, taken};
use super::event::{Event, Subscription};
use super::mcp::{HostPlan, Labels, PRIMARY};
use super::mcp_placement::Placement;
use super::mcp_placement::fake::{Behavior, Fake};
use super::services_tests::{ERR, VAR, builder, config, of_kind, quick};
use super::{
    McpPlacementKind, McpPolicy, McpServiceOutcome, McpServiceSpec, McpStopHow, McpWrites, Quiesce,
    QuiesceOutcome, ServicePolicy, Session, SessionBuilder, Stopped, Verdict,
};
use crate::config::{EnvValue, McpServerSpec, SidecarView, SidecarWorkspaceAccess};
use crate::error::McpFailureKind;
use crate::outrig_::{ConfiguredMcp, ConfiguredSidecar};
use crate::python::testing::within;
use crate::{MountSpec, SidecarServerSpec, SidecarSpec};
use tokio_util::sync::CancellationToken;

/// The fake placements of a session, by the container each stands for: [`PRIMARY`], or a
/// sidecar's name -- `mcp-<service>` for an owner's.
#[derive(Clone, Default)]
pub(super) struct Fakes {
    placed: Arc<Mutex<HashMap<String, Arc<Fake>>>>,
    /// Each sidecar placed, by name, and the image it was placed from.
    images: Arc<Mutex<Vec<(String, String)>>>,
}

impl Fakes {
    /// The fake for `host`, made if there is none yet.
    pub(super) fn of(&self, host: &str) -> Arc<Fake> {
        Arc::clone(
            self.placed
                .lock()
                .unwrap()
                .entry(host.to_string())
                .or_default(),
        )
    }

    /// `builder`, its MCP servers placed by these fakes.
    pub(super) fn place(&self, builder: SessionBuilder) -> SessionBuilder {
        let fakes = self.clone();
        builder.mcp_placements(move |planned| {
            let host = match planned {
                HostPlan::Primary => PRIMARY,
                HostPlan::Sidecar(spec) => {
                    let placed = (spec.name.clone(), spec.image.clone());
                    fakes.images.lock().unwrap().push(placed);
                    &spec.name
                }
            };
            fakes.of(host) as Arc<dyn Placement>
        })
    }
}

/// A policy whose attempts take at most `attempt`, within `budget`.
pub(super) fn policy(attempt: Duration, budget: Duration) -> McpPolicy {
    McpPolicy {
        attempt,
        ..McpPolicy::default().with_connect_budget(budget)
    }
}

/// A session started from `builder`, its servers placed by `fakes`, with quick calls and the
/// policy's attempts quick too; its events subscribed to, and `err` defined in its primary.
async fn placed(fakes: &Fakes, builder: SessionBuilder) -> (Session, Subscription) {
    placed_under(
        fakes,
        builder,
        quick(Duration::from_secs(3)),
        policy(Duration::from_millis(500), Duration::from_secs(5)),
    )
    .await
}

/// [`placed`], its calls under `calls` and its starts under `starts`.
pub(super) async fn placed_under(
    fakes: &Fakes,
    builder: SessionBuilder,
    calls: ServicePolicy,
    starts: McpPolicy,
) -> (Session, Subscription) {
    let mut builder = fakes
        .place(builder)
        .service_policy(calls)
        .mcp_policy(starts);
    let events = builder.subscribe();
    let session = start(builder).await;
    output(&session, ERR).await;
    (session, events)
}

/// A sidecar with the primary's view, hosting its entrypoint as its one server, with `env`.
fn viewing(env: &[(&str, &str)]) -> SidecarSpec {
    let env = env
        .iter()
        .map(|(key, value)| (key.to_string(), EnvValue::from_raw(value.to_string())))
        .collect();
    SidecarSpec::from_image("any", "mcp-image")
        .with_view(SidecarView::Primary)
        .with_server_spec("server", SidecarServerSpec::entrypoint(["/"]).with_env(env))
}

/// A sidecar of `mcp-image` hosting one exec'd server, with `env`.
pub(super) fn sidecar(env: &[(&str, &str)]) -> SidecarSpec {
    let env = env
        .iter()
        .map(|(key, value)| (key.to_string(), EnvValue::from_raw(value.to_string())))
        .collect();
    SidecarSpec::from_image("any", "mcp-image").with_server_env(
        "server",
        vec!["gh-server".to_string()],
        env,
    )
}

/// `name`'s outcome in `session`.
pub(super) fn outcome(session: &Session, name: &str) -> McpServiceOutcome {
    session
        .mcp()
        .outcomes()
        .into_iter()
        .find(|(service, _)| service == name)
        .unwrap_or_else(|| panic!("no outcome for {name}"))
        .1
}

/// The `data` of each `mcp.service.*` event of `kind` naming `service`.
pub(super) fn of_service(events: &[Arc<Event>], kind: &str, service: &str) -> Vec<Value> {
    of_kind(events, kind)
        .into_iter()
        .filter(|data| data["service"] == service)
        .collect()
}

/// Where in `events` the first event of `kind` naming `service` is.
pub(super) fn position(events: &[Arc<Event>], kind: &str, service: &str) -> usize {
    events
        .iter()
        .position(|event| {
            event.kind() == kind
                && serde_json::to_value(&event.payload).expect("encodes")["service"] == service
        })
        .unwrap_or_else(|| panic!("no {kind} for {service}"))
}

/// A command, a sidecar and a primary view start through one facility: each ready with its
/// listing, announced, bound as an `mcp` service, listed and callable.
#[tokio::test]
async fn three_placements_start_through_one_facility() {
    let fakes = Fakes::default();
    let (session, mut events) = placed(
        &fakes,
        builder()
            .mcp("cmd", McpServiceSpec::command(["fs-server"]))
            .mcp("side", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp(
                "view",
                McpServiceSpec::primary_view("mcp-image").with_args(["/"]),
            ),
    )
    .await;
    for name in ["cmd", "side", "view"] {
        assert_eq!(
            outcome(&session, name),
            McpServiceOutcome::Ready {
                tools: 3,
                listing: 1,
                undecoded: Vec::new(),
                attempts: 1
            },
            "{name}"
        );
    }
    assert_eq!(
        output(
            &session,
            "assert runtime.facilities == runtime.bindings\nprint(sorted(runtime.facilities))"
        )
        .await,
        "['cmd', 'side', 'view']\n"
    );
    assert_eq!(
        output(
            &session,
            "print(outrig.bindings['side'].echo(text='hi').content[0].text, \
             view.call('echo', {'text': 'there'}).content[0].text)",
        )
        .await,
        "hi there\n"
    );
    let report = stop(session).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let events = taken(&mut events);
    for (name, placement) in [
        ("cmd", "command"),
        ("side", "sidecar"),
        ("view", "primary_view"),
    ] {
        let started = of_service(&events, "mcp.service.started", name);
        assert_eq!(started.len(), 1, "{name}");
        assert_eq!(started[0]["placement"], placement);
        assert_eq!(started[0]["declared_in"], "owner");
        assert_eq!(started[0]["tools"], 3);
        let bound = of_service(&events, "service.bound", name);
        assert_eq!(bound[0]["kind"], "mcp", "{name}");
        assert!(
            position(&events, "mcp.service.started", name)
                < position(&events, "service.bound", name)
        );
    }
    assert_eq!(fakes.of(PRIMARY).connects(), 1);
    assert_eq!(fakes.of("mcp-side").connects(), 1);
    assert_eq!(fakes.of("mcp-view").connects(), 1);
}

/// Two picks of one declaration with different credentials are two services in two containers,
/// each started with its own value; no value appears in anything an owner or an agent reads,
/// and a failure shows the command with the reference as written.
#[tokio::test]
async fn two_picks_coexist_and_their_values_cross_nothing() {
    let fakes = Fakes::default();
    fakes
        .of("mcp-gh_c")
        .script("gh_c", [Behavior::Exit(127, "gh-server: not found")]);
    let secrets = |var: &str| match var {
        "A" => Some("sk-pick-a-5f1e".to_string()),
        "B" => Some("sk-pick-b-9c3d".to_string()),
        VAR => Some("sk-test".to_string()),
        _ => None,
    };
    let (session, mut events) = placed(
        &fakes,
        builder()
            .secrets(secrets)
            .mcp(
                "gh_a",
                McpServiceSpec::sidecar(sidecar(&[("TOKEN", "${A}")])),
            )
            .mcp(
                "gh_b",
                McpServiceSpec::sidecar(sidecar(&[("TOKEN", "${B}")])),
            )
            .mcp(
                "gh_c",
                McpServiceSpec::sidecar(sidecar(&[("TOKEN", "${A}")])),
            ),
    )
    .await;
    let token = |host: &str| fakes.of(host).envs()[0].1["TOKEN"].clone();
    assert_eq!(token("mcp-gh_a"), "sk-pick-a-5f1e");
    assert_eq!(token("mcp-gh_b"), "sk-pick-b-9c3d");
    let failed = outcome(&session, "gh_c");
    assert_eq!(
        failed.cause().and_then(|cause| cause.command.as_deref()),
        Some("TOKEN=${A} gh-server")
    );
    let outcomes = session.mcp().outcomes();
    let shown = [
        serde_json::to_string(&outcomes).unwrap(),
        format!("{outcomes:?}"),
        format!("{:?}", session.mcp()),
        output(&session, "print(repr(gh_a), help(gh_a))").await,
    ];
    let report = stop(session).await;
    let events = taken(&mut events);
    let read: Vec<String> = shown
        .into_iter()
        .chain([
            serde_json::to_string(&report).unwrap(),
            format!("{report:?}"),
        ])
        .chain(
            events
                .iter()
                .map(|event| serde_json::to_string(&event.payload).unwrap()),
        )
        .collect();
    for text in &read {
        assert!(
            !text.contains("sk-pick-a") && !text.contains("sk-pick-b"),
            "a value crossed: {text}"
        );
    }
}

/// A server whose binary is not there is misconfigured after one attempt, its outcome naming
/// the service, the exit and what it said; nothing is retried.
#[tokio::test]
async fn a_missing_binary_names_the_declaration() {
    let fakes = Fakes::default();
    fakes
        .of(PRIMARY)
        .script("fs", [Behavior::Exit(127, "sh: fs-server: not found")]);
    let (session, mut events) = placed(
        &fakes,
        builder().mcp("fs", McpServiceSpec::command(["fs-server"])),
    )
    .await;
    let McpServiceOutcome::Misconfigured(cause) = outcome(&session, "fs") else {
        panic!("{:?}", outcome(&session, "fs"))
    };
    assert_eq!(cause.service, "fs");
    assert_eq!(cause.exit.as_deref(), Some("code 127"));
    assert_eq!(
        cause.stderr_tail.as_deref(),
        Some("sh: fs-server: not found")
    );
    assert_eq!(cause.attempts, 1);
    assert_eq!(
        output(&session, "print('fs' in runtime.bindings)").await,
        "False\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    assert!(of_kind(&events, "mcp.service.retry").is_empty());
    let failed = of_service(&events, "mcp.service.failed", "fs");
    assert_eq!(failed[0]["outcome"], "misconfigured");
    assert_eq!(fakes.of(PRIMARY).connects(), 1);
}

/// A handshake that never answers is tried again within the budget: a second attempt that
/// answers is ready, after one retry; one that never answers is unavailable once the budget
/// is spent, each attempt bounded.
#[tokio::test]
async fn transient_failures_are_retried_within_the_budget() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("late", [Behavior::Hang]);
    fakes.of(PRIMARY).script("never", vec![Behavior::Hang; 20]);
    let mut builder = fakes
        .place(
            builder()
                .mcp("late", McpServiceSpec::command(["late-server"]))
                .mcp("never", McpServiceSpec::command(["never-server"])),
        )
        .mcp_policy(policy(
            Duration::from_millis(300),
            Duration::from_millis(1500),
        ));
    let mut events = builder.subscribe();
    let started = Instant::now();
    let session = start(builder).await;
    let took = started.elapsed();
    assert_eq!(
        outcome(&session, "late"),
        McpServiceOutcome::Ready {
            tools: 3,
            listing: 1,
            undecoded: Vec::new(),
            attempts: 2
        }
    );
    let McpServiceOutcome::Unavailable(cause) = outcome(&session, "never") else {
        panic!("{:?}", outcome(&session, "never"))
    };
    assert!(cause.attempts >= 2, "{cause:?}");
    assert!(cause.message.contains("connect budget"), "{cause:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");
    stop(session).await;
    let events = taken(&mut events);
    let retries = of_service(&events, "mcp.service.retry", "late");
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(retries[0]["cause"]["kind"], "timeout");
    assert_eq!(retries[0]["delay"], 0.5);
    let never = of_service(&events, "mcp.service.retry", "never");
    assert_eq!(never.len() as u32 + 1, cause.attempts);
    assert_eq!(
        fakes.of(PRIMARY).connects() as u32,
        2 + cause.attempts,
        "each attempt one connect"
    );
}

/// An attempt whose container's preparation never finishes is held to the connect budget all
/// the same: the session starts without it once the budget is spent.
#[tokio::test]
async fn a_wedged_preparation_is_held_to_the_budget() {
    let fakes = Fakes::default();
    fakes
        .of(PRIMARY)
        .script("wedged", vec![Behavior::Wedged; 5]);
    let builder = fakes
        .place(builder().mcp("wedged", McpServiceSpec::command(["wedged-server"])))
        .mcp_policy(policy(Duration::from_millis(300), Duration::from_secs(1)));
    let started = Instant::now();
    let session = tokio::time::timeout(Duration::from_secs(10), start(builder))
        .await
        .expect("the session starts once the budget is spent");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let McpServiceOutcome::Unavailable(cause) = outcome(&session, "wedged") else {
        panic!("{:?}", outcome(&session, "wedged"))
    };
    assert_eq!(cause.kind, Some(McpFailureKind::Timeout));
    assert!(cause.message.contains("connect budget"), "{cause:?}");
    stop(session).await;
}

/// A listing that never answers is held to the attempt's bound, not the whole budget: the
/// attempt fails, and the next is ready.
#[tokio::test]
async fn a_listing_is_held_to_the_attempts_bound() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("unlisted", [Behavior::Unlisted]);
    let builder = fakes
        .place(builder().mcp("unlisted", McpServiceSpec::command(["unlisted-server"])))
        .mcp_policy(policy(Duration::from_millis(300), Duration::from_secs(30)));
    let started = Instant::now();
    let session = start(builder).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        outcome(&session, "unlisted"),
        McpServiceOutcome::Ready {
            tools: 3,
            listing: 1,
            undecoded: Vec::new(),
            attempts: 2
        }
    );
    stop(session).await;
}

/// A listing refused by a server that then ignores its closed stdin is that refusal, though
/// closing its transport outlasts the budget: not a start that ran out of time.
#[tokio::test]
async fn a_refused_listing_outlasted_by_its_close_is_the_refusal() {
    const UNLISTING: &str = r#"answer() {
            read -r line
            id=$(printf '%s' "$line" | grep -o '"id":[0-9]*' | head -n 1 | cut -d: -f2)
            printf '{"jsonrpc":"2.0","id":%s,%s}\n' "$id" "$1"
        }
        answer '"result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"s","version":"0"}}'
        read -r initialized
        answer '"error":{"code":-32603,"message":"no listing"}'
        exec sleep 10"#;
    let fakes = Fakes::default();
    fakes
        .of(PRIMARY)
        .script("unlisting", [Behavior::Script(UNLISTING)]);
    let builder = fakes
        .place(builder().mcp("unlisting", McpServiceSpec::command(["unlisting-server"])))
        .mcp_policy(policy(
            Duration::from_millis(1500),
            Duration::from_millis(1500),
        ));
    let session = start(builder).await;
    let McpServiceOutcome::Unclassified(cause) = outcome(&session, "unlisting") else {
        panic!("{:?}", outcome(&session, "unlisting"))
    };
    assert_eq!(cause.kind, Some(McpFailureKind::Protocol), "{cause:?}");
    assert!(cause.message.contains("no listing"), "{cause:?}");
    stop(session).await;
}

/// A sidecar of one server's own whose stop between attempts runs past the budget starts no
/// attempt after it: the start fails as the attempt before the stop did, with no retry said.
#[tokio::test]
async fn a_stop_past_the_budget_starts_nothing_more() {
    let fakes = Fakes::default();
    fakes.of("mcp-side").script("side", [Behavior::Hang]);
    fakes.of("mcp-side").slow_stop(Duration::from_secs(1));
    let mut builder = fakes
        .place(builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))))
        .mcp_policy(policy(Duration::from_millis(300), Duration::from_secs(1)));
    let mut events = builder.subscribe();
    let session = start(builder).await;
    let McpServiceOutcome::Unavailable(cause) = outcome(&session, "side") else {
        panic!("{:?}", outcome(&session, "side"))
    };
    assert_eq!(cause.attempts, 1, "{cause:?}");
    assert!(cause.message.contains("budget is spent"), "{cause:?}");
    assert_eq!(cause.kind, Some(McpFailureKind::Timeout), "{cause:?}");
    let events = taken(&mut events);
    assert!(of_service(&events, "mcp.service.retry", "side").is_empty());
    stop(session).await;
}

/// A refusal that takes the whole of its attempt's bound to report -- its diagnosis waiting as
/// long as the handshake had left -- is a refusal still when the budget is what bounds the
/// attempt, not a start that ran out of time: the attempt keeps a moment back for it.
#[tokio::test]
async fn a_refusal_at_the_bound_is_a_refusal() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("late", [Behavior::RefuseAtBound]);
    let builder = fakes
        .place(builder().mcp("late", McpServiceSpec::command(["late-server"])))
        .mcp_policy(policy(Duration::from_secs(2), Duration::from_secs(1)));
    let session = start(builder).await;
    let late = outcome(&session, "late");
    assert!(matches!(late, McpServiceOutcome::Refused(_)), "{late:?}");
    stop(session).await;
}

/// A server that answers `initialize` with an error is refused after one attempt; one that
/// exits in a way OutRig does not know is unclassified, said so.
#[tokio::test]
async fn a_refusal_is_definitive() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("no", [Behavior::Refuse]);
    fakes
        .of(PRIMARY)
        .script("odd", [Behavior::Exit(1, "something broke")]);
    let (session, mut events) = placed(
        &fakes,
        builder()
            .mcp("no", McpServiceSpec::command(["no-server"]))
            .mcp("odd", McpServiceSpec::command(["odd-server"])),
    )
    .await;
    let McpServiceOutcome::Refused(cause) = outcome(&session, "no") else {
        panic!("{:?}", outcome(&session, "no"))
    };
    assert_eq!(cause.attempts, 1);
    assert!(cause.message.contains("not welcome"), "{cause:?}");
    let McpServiceOutcome::Unclassified(cause) = outcome(&session, "odd") else {
        panic!("{:?}", outcome(&session, "odd"))
    };
    assert_eq!(cause.attempts, 1);
    stop(session).await;
    let events = taken(&mut events);
    assert!(of_kind(&events, "mcp.service.retry").is_empty());
    assert_eq!(
        of_service(&events, "mcp.service.failed", "odd")[0]["outcome"],
        "unclassified"
    );
}

/// A credential is not placed where agent code can read it: a `${VAR}` in a command's or a
/// primary view's env, or a sidecar's whose view is the primary's, is misconfigured before
/// anything connects; in a sidecar of its own it starts. A reference under a key podman reads
/// itself is misconfigured too.
#[tokio::test]
async fn the_credential_rule() {
    let fakes = Fakes::default();
    let a = || EnvValue::from_raw("${A}".to_string());
    let (session, _events) = placed(
        &fakes,
        builder()
            .secrets(|var: &str| Some(format!("value-of-{var}")))
            .mcp("plain", McpServiceSpec::command(["plain-server"]))
            .mcp(
                "cmd",
                McpServiceSpec::command(["fs-server"]).with_env("TOKEN", a()),
            )
            .mcp(
                "view",
                McpServiceSpec::primary_view("mcp-image").with_env("TOKEN", a()),
            )
            .mcp(
                "side",
                McpServiceSpec::sidecar(sidecar(&[("TOKEN", "${A}")])),
            )
            .mcp(
                "viewing",
                McpServiceSpec::sidecar(viewing(&[("TOKEN", "${A}")])),
            )
            .mcp(
                "pathed",
                McpServiceSpec::sidecar(sidecar(&[("PATH", "${A}")])),
            ),
    )
    .await;
    for name in ["cmd", "view", "viewing"] {
        let McpServiceOutcome::Misconfigured(cause) = outcome(&session, name) else {
            panic!("{name}: {:?}", outcome(&session, name))
        };
        assert!(cause.message.contains("holds a credential"), "{cause:?}");
        assert_eq!(cause.attempts, 0);
    }
    let McpServiceOutcome::Misconfigured(cause) = outcome(&session, "pathed") else {
        panic!("{:?}", outcome(&session, "pathed"))
    };
    assert!(cause.message.contains("podman reads itself"), "{cause:?}");
    assert!(matches!(
        outcome(&session, "side"),
        McpServiceOutcome::Ready { .. }
    ));
    assert_eq!(fakes.of(PRIMARY).connects(), 1, "only the plain command");
    for host in ["mcp-view", "mcp-viewing", "mcp-pathed"] {
        assert_eq!(fakes.of(host).connects(), 0, "{host}");
    }
    stop(session).await;
}

/// A configuration's sidecar block named as an owner's service's container is not placed in
/// its stead -- its servers are misconfigured, and the owner's server runs, with its values, in
/// what the owner declared -- and one named `primary` is a sidecar like any other.
#[tokio::test]
async fn a_configured_sidecar_never_takes_an_owners_place() {
    let fakes = Fakes::default();
    let configured = ConfiguredMcp {
        sidecars: vec![
            ConfiguredSidecar {
                spec: SidecarSpec::from_image("mcp-gh", "repo-image")
                    .with_server("lookalike", vec!["lookalike-server".to_string()]),
                failed: None,
            },
            ConfiguredSidecar {
                spec: SidecarSpec::from_image("primary", "tools-image")
                    .with_server("tools", vec!["tools-server".to_string()]),
                failed: None,
            },
        ],
        names: ["lookalike", "tools"]
            .into_iter()
            .map(String::from)
            .collect(),
        ..ConfiguredMcp::default()
    };
    let (session, _events) = placed(
        &fakes,
        builder()
            .secrets(|var: &str| Some(format!("value-of-{var}")))
            .mcp("gh", McpServiceSpec::sidecar(sidecar(&[("TOKEN", "${A}")])))
            .mcp("cmd", McpServiceSpec::command(["fs-server"]))
            .mcp_configured(configured, Labels::default()),
    )
    .await;
    for name in ["gh", "cmd", "tools"] {
        assert!(
            matches!(outcome(&session, name), McpServiceOutcome::Ready { .. }),
            "{name}: {:?}",
            outcome(&session, name)
        );
    }
    let McpServiceOutcome::Misconfigured(cause) = outcome(&session, "lookalike") else {
        panic!("{:?}", outcome(&session, "lookalike"))
    };
    assert!(cause.message.contains("\"mcp-gh\""), "{cause:?}");
    assert_eq!(cause.attempts, 0);
    let images = fakes.images.lock().unwrap().clone();
    assert!(
        images.contains(&("mcp-gh".to_string(), "mcp-image".to_string())),
        "{images:?}"
    );
    assert!(
        images.iter().all(|(_, image)| image != "repo-image"),
        "{images:?}"
    );
    assert_eq!(fakes.of("mcp-gh").envs()[0].1["TOKEN"], "value-of-A");
    assert_eq!(
        fakes.of(PRIMARY).connects(),
        1,
        "the command, in the primary"
    );
    assert_eq!(
        fakes.of("primary").connects(),
        1,
        "the block's server, in it"
    );
    stop(session).await;
}

/// A name given twice, or one a binding or a service takes, fails the start before anything
/// starts.
#[test]
fn a_name_taken_fails_the_start() {
    let twice = builder()
        .mcp("fs", McpServiceSpec::command(["a"]))
        .mcp("fs", McpServiceSpec::command(["b"]));
    let error = twice.check().unwrap_err().to_string();
    assert!(error.contains("given through `mcp` already"), "{error}");
    let served = builder()
        .serve("svc", super::services_tests::Fixture::new())
        .mcp("svc", McpServiceSpec::command(["a"]));
    let error = served.check().unwrap_err().to_string();
    assert!(error.contains("service's or a channel's name"), "{error}");
    let bound = builder()
        .bind(
            "repo",
            crate::config::Binding::new("os:getcwd", "A binding."),
        )
        .mcp("repo", McpServiceSpec::command(["a"]));
    let error = bound.check().unwrap_err().to_string();
    assert!(error.contains("is a binding's name already"), "{error}");
    let keyword = builder().mcp("class", McpServiceSpec::command(["a"]));
    assert!(keyword.check().is_err());
    let launched = builder()
        .container(
            crate::LaunchSpec::from_image("img", BTreeMap::new(), std::env::temp_dir())
                .with_sidecar(SidecarSpec::from_image("mcp-fs", "img")),
        )
        .mcp("fs", McpServiceSpec::sidecar(sidecar(&[])));
    let error = launched.check().unwrap_err().to_string();
    assert!(error.contains("`mcp-fs`"), "{error}");
}

/// A configuration's block named as a sidecar the launch starts is not placed: its servers are
/// misconfigured.
#[test]
fn a_configured_sidecar_never_takes_a_launched_ones_name() {
    let configured = ConfiguredMcp {
        sidecars: vec![ConfiguredSidecar {
            spec: SidecarSpec::from_image("tools", "tools-image")
                .with_server("grep", vec!["grep-server".to_string()]),
            failed: None,
        }],
        ..ConfiguredMcp::default()
    };
    let plan = super::mcp::Plan::new(
        &[],
        Some(&configured),
        Labels::default(),
        |_| false,
        None,
        ["tools".to_string()].into(),
        &[],
    );
    let why = plan.services[0]
        .misconfigured
        .as_deref()
        .unwrap_or_default();
    assert!(why.contains("\"tools\"") && why.contains("launch"), "{why}");
    assert!(!plan.hosts.contains_key("tools"));
}

/// An owner's connect budget is held to the configuration's range.
#[test]
fn a_connect_budget_out_of_range_fails_the_start() {
    for budget in [Duration::ZERO, Duration::from_secs(3601)] {
        let error = builder()
            .mcp_policy(McpPolicy::default().with_connect_budget(budget))
            .check()
            .unwrap_err()
            .to_string();
        assert!(error.contains("from 1 to 3600 seconds"), "{error}");
    }
}

/// Each `${VAR}` is asked of the session's resolver once, however many services name it, and
/// the process environment is never read for it.
#[tokio::test]
async fn secrets_go_through_the_resolver() {
    let fakes = Fakes::default();
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let counter = Arc::clone(&asked);
    let secrets = move |var: &str| {
        counter.lock().unwrap().push(var.to_string());
        (var == VAR || var == "OUTRIG_TEST_MCP_ONLY_IN_SECRETS").then(|| format!("v-{var}"))
    };
    let (session, _events) = placed(
        &fakes,
        builder()
            .secrets(secrets)
            .mcp(
                "one",
                McpServiceSpec::sidecar(sidecar(&[("T", "${OUTRIG_TEST_MCP_ONLY_IN_SECRETS}")])),
            )
            .mcp(
                "two",
                McpServiceSpec::sidecar(sidecar(&[("T", "${OUTRIG_TEST_MCP_ONLY_IN_SECRETS}")])),
            )
            .mcp(
                "three",
                McpServiceSpec::sidecar(sidecar(&[("T", "${UNSET}")])),
            ),
    )
    .await;
    assert!(matches!(
        outcome(&session, "one"),
        McpServiceOutcome::Ready { .. }
    ));
    assert!(matches!(
        outcome(&session, "two"),
        McpServiceOutcome::Ready { .. }
    ));
    let McpServiceOutcome::Misconfigured(cause) = outcome(&session, "three") else {
        panic!("{:?}", outcome(&session, "three"))
    };
    assert!(cause.message.contains("${UNSET}"), "{cause:?}");
    let asked = asked.lock().unwrap().clone();
    let count = |var: &str| asked.iter().filter(|asked| *asked == var).count();
    assert_eq!(count("OUTRIG_TEST_MCP_ONLY_IN_SECRETS"), 1, "{asked:?}");
    assert_eq!(count("UNSET"), 1, "{asked:?}");
    assert_eq!(
        fakes.of("mcp-one").envs()[0].1["T"],
        "v-OUTRIG_TEST_MCP_ONLY_IN_SECRETS"
    );
    stop(session).await;
}

/// A configuration's entries become declarations: a primary command, a server whose name holds
/// `-` bound with `_` and its own name kept, a label entry, and two servers sharing a sidecar
/// that holds a credential.
#[tokio::test]
async fn configuration_lowers_into_declarations() {
    let fakes = Fakes::default();
    let tools = SidecarSpec::from_image("tools", "tools-image")
        .with_server_env(
            "gh",
            vec!["gh-server".to_string()],
            BTreeMap::from([("TOKEN".to_string(), EnvValue::from_raw("${A}".into()))]),
        )
        .with_server("grep", vec!["grep-server".to_string()]);
    let configured = ConfiguredMcp {
        primary: BTreeMap::from([
            ("fs".to_string(), McpServerSpec::exec(["fs-server"])),
            ("my-fs".to_string(), McpServerSpec::exec(["my-fs-server"])),
        ]),
        sidecars: vec![ConfiguredSidecar {
            spec: tools,
            failed: None,
        }],
        names: ["fs", "my-fs", "gh", "grep"]
            .into_iter()
            .map(String::from)
            .collect(),
        ..ConfiguredMcp::default()
    };
    let labels = Labels {
        image: "primary-image".into(),
        servers: BTreeMap::from([
            ("lbl".to_string(), McpServerSpec::exec(["lbl-server"])),
            ("fs".to_string(), McpServerSpec::exec(["not-this-one"])),
        ]),
    };
    let (session, mut events) = placed(
        &fakes,
        builder()
            .secrets(|var: &str| Some(format!("value-of-{var}")))
            .mcp_configured(configured, labels),
    )
    .await;
    let names: Vec<String> = session
        .mcp()
        .outcomes()
        .into_iter()
        .map(|(name, outcome)| {
            assert!(
                matches!(outcome, McpServiceOutcome::Ready { .. }),
                "{name}: {outcome:?}"
            );
            name
        })
        .collect();
    assert_eq!(names, ["fs", "my_fs", "lbl", "gh", "grep"]);
    assert_eq!(
        fakes.of("tools").connects(),
        2,
        "one container, two servers"
    );
    stop(session).await;
    let events = taken(&mut events);
    let started = of_service(&events, "mcp.service.started", "my_fs");
    assert_eq!(started[0]["server"], "my-fs");
    assert_eq!(started[0]["declared_in"], "config");
    assert_eq!(
        of_service(&events, "mcp.service.started", "lbl")[0]["declared_in"],
        "image_label"
    );
    assert_eq!(
        of_service(&events, "mcp.service.started", "gh")[0]["placement"],
        "sidecar"
    );
    let fs_env = &fakes.of(PRIMARY).envs();
    assert!(
        fs_env.iter().all(|(server, _)| server != "not-this-one"),
        "the configuration's entry wins over the label's"
    );
}

/// Which services write what, read from their declarations alone.
#[tokio::test]
async fn writers_are_read_from_the_declarations() {
    let fakes = Fakes::default();
    let workspace = tempfile::tempdir().unwrap();
    let mount = tempfile::tempdir().unwrap();
    let (session, _events) = placed(
        &fakes,
        builder()
            .host_workspace(workspace.path())
            .mcp("cmd", McpServiceSpec::command(["a"]))
            .mcp("view", McpServiceSpec::primary_view("mcp-image"))
            .mcp(
                "rw",
                McpServiceSpec::sidecar(
                    sidecar(&[]).with_workspace_access(SidecarWorkspaceAccess::Rw),
                ),
            )
            .mcp(
                "ro",
                McpServiceSpec::sidecar(
                    sidecar(&[]).with_workspace_access(SidecarWorkspaceAccess::Ro),
                ),
            )
            .mcp(
                "mounted",
                McpServiceSpec::sidecar(sidecar(&[]).with_mount(MountSpec::new(
                    mount.path(),
                    "/data",
                    crate::config::MountAccess::ReadWrite,
                ))),
            )
            .mcp("viewing", McpServiceSpec::sidecar(viewing(&[]))),
    )
    .await;
    let writers: HashMap<String, McpWrites> = session
        .mcp()
        .writers()
        .into_iter()
        .map(|writer| (writer.service, writer.writes))
        .collect();
    assert_eq!(writers["cmd"], McpWrites::PrimaryFilesystem);
    assert_eq!(writers["view"], McpWrites::PrimaryFilesystem);
    assert_eq!(writers["viewing"], McpWrites::PrimaryFilesystem);
    assert_eq!(
        writers["rw"],
        McpWrites::Paths(vec![workspace.path().to_path_buf()])
    );
    assert_eq!(writers["ro"], McpWrites::Nothing);
    assert_eq!(
        writers["mounted"],
        McpWrites::Paths(vec![mount.path().to_path_buf()])
    );
    stop(session).await;
}

/// An owner's pause freezes the sidecar and refuses a call meanwhile, reason paused; the
/// resume thaws it and the next call works. A cut's pause holds a call until the resume. A
/// call still running when the wait runs out leaves the container unfrozen, named.
#[tokio::test]
async fn a_pause_holds_dispatch_and_freezes() {
    let fakes = Fakes::default();
    let (session, mut events) = placed(
        &fakes,
        builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let fake = fakes.of("mcp-side");
    let mcp = session.mcp().clone();
    let paused = mcp
        .quiesce("side", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert_eq!(
        paused,
        QuiesceOutcome::Quiesced {
            how: Quiesce::Pause,
            services: vec!["side".into()]
        }
    );
    assert!(fake.frozen.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        output(&session, "err(side.echo, text='x')").await,
        "refused paused paused None\n"
    );
    assert_eq!(mcp.resume("side").await, Ok(vec!["side".to_string()]));
    assert_eq!(fake.thaws(), 1);
    assert_eq!(
        output(&session, "print(side.echo(text='back').content[0].text)").await,
        "back\n"
    );

    let until = std::time::Instant::now() + Duration::from_secs(30);
    let held = mcp
        .quiesce(
            "side",
            Quiesce::pause_holding(until),
            Duration::from_secs(1),
        )
        .await;
    assert!(matches!(held, QuiesceOutcome::Quiesced { .. }), "{held:?}");
    let mut call = session
        .interpreter()
        .submit("print(side.echo(text='held').content[0].text)")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), call.outcome())
            .await
            .is_err(),
        "the call waits for the resume"
    );
    mcp.resume("side").await.unwrap();
    let ran = within(call.outcome()).await;
    assert!(format!("{ran:?}").contains("held"), "{ran:?}");

    let mut slow = session.interpreter().submit("err(side.slow)").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = mcp
        .quiesce("side", Quiesce::Pause, Duration::from_millis(200))
        .await;
    let QuiesceOutcome::InFlight { calls, services } = busy else {
        panic!("{busy:?}")
    };
    assert_eq!((calls.len(), services), (1, vec!["side".to_string()]));
    assert_eq!(fake.freezes(), 2, "nothing frozen while the call ran");
    mcp.resume("side").await.unwrap();
    within(slow.outcome()).await;
    stop(session).await;
    let events = taken(&mut events);
    let pauses = of_service(&events, "mcp.service.paused", "side");
    assert_eq!(
        pauses
            .iter()
            .map(|p| (p["how"].clone(), p["proven"].clone()))
            .collect::<Vec<_>>(),
        [
            (Value::from("refuse"), Value::from(true)),
            (Value::from("hold"), Value::from(true)),
            (Value::from("refuse"), Value::from(false)),
        ]
    );
    assert_eq!(of_service(&events, "mcp.service.resumed", "side").len(), 3);
}

/// A pause asked of a service already paused sets the hold it asks for: a cut's pause after an
/// owner's makes a call wait, and an owner's after a cut's refuses the call waiting. The
/// container is frozen once.
#[tokio::test]
async fn a_second_pause_sets_its_own_hold() {
    let fakes = Fakes::default();
    let (session, mut events) = placed(
        &fakes,
        builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let mcp = session.mcp().clone();
    mcp.quiesce("side", Quiesce::Pause, Duration::from_secs(1))
        .await;
    let until = std::time::Instant::now() + Duration::from_secs(30);
    let held = mcp
        .quiesce(
            "side",
            Quiesce::pause_holding(until),
            Duration::from_secs(1),
        )
        .await;
    assert_eq!(
        held,
        QuiesceOutcome::Quiesced {
            how: Quiesce::pause_holding(until),
            services: vec!["side".into()]
        }
    );
    let mut call = session
        .interpreter()
        .submit("err(side.echo, text='x')")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), call.outcome())
            .await
            .is_err(),
        "the call waits"
    );
    mcp.quiesce("side", Quiesce::Pause, Duration::from_secs(1))
        .await;
    let ran = within(call.outcome()).await;
    assert!(
        format!("{ran:?}").contains("refused paused paused None"),
        "{ran:?}"
    );
    assert_eq!(fakes.of("mcp-side").freezes(), 1, "frozen once");
    mcp.resume("side").await.unwrap();
    stop(session).await;
    let events = taken(&mut events);
    let hows: Vec<Value> = of_service(&events, "mcp.service.paused", "side")
        .into_iter()
        .map(|paused| paused["how"].clone())
        .collect();
    assert_eq!(hows, ["refuse", "hold", "refuse"]);
}

/// A call that runs past its deadline is told to stop and, its grace out, abandoned: it ends
/// unknown, reason deadline, since its server may have done the work -- not raised by a
/// deadline of the client's own, which would end it inside the grace.
#[tokio::test]
async fn a_call_past_its_deadline_ends_unknown() {
    let fakes = Fakes::default();
    let mut builder = fakes
        .place(builder().mcp(
            "side",
            McpServiceSpec::sidecar(sidecar(&[])).with_call_timeout_secs(1),
        ))
        .service_policy(ServicePolicy {
            cancel_grace: Duration::from_secs(2),
            ..quick(Duration::from_secs(3))
        })
        .mcp_policy(policy(Duration::from_millis(500), Duration::from_secs(5)));
    let mut events = builder.subscribe();
    let session = start(builder).await;
    output(&session, ERR).await;
    assert_eq!(
        output(&session, "err(side.slow)").await,
        "unknown None deadline None\n"
    );
    stop(session).await;
    let events = taken(&mut events);
    let ended = of_kind(&events, "service.call.ended")
        .into_iter()
        .find(|ended| ended["method"] == "slow")
        .expect("the call ended");
    assert_eq!(
        (&ended["outcome"], &ended["reason"]),
        (&Value::from("unknown"), &Value::from("deadline"))
    );
}

/// A stop tells the call in flight to stop -- it ends unknown, reason stopped -- closes the
/// transport and stops the container; a later call is refused. A command cannot be quiesced,
/// and a container that will not freeze says why.
#[tokio::test]
async fn a_stop_ends_calls_and_the_container() {
    let fakes = Fakes::default();
    fakes.of("mcp-stuck").fail_freeze("the freezer is broken");
    let (session, mut events) = placed(
        &fakes,
        builder()
            .mcp("side", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp("stuck", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp("cmd", McpServiceSpec::command(["a"])),
    )
    .await;
    let mcp = session.mcp().clone();
    let mut slow = session.interpreter().submit("err(side.slow)").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped = mcp.quiesce("side", Quiesce::Stop, Duration::ZERO).await;
    assert!(
        matches!(stopped, QuiesceOutcome::Quiesced { .. }),
        "{stopped:?}"
    );
    assert!(
        fakes
            .of("mcp-side")
            .stopped
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    let ran = within(slow.outcome()).await;
    assert!(
        format!("{ran:?}").contains("unknown None stopped"),
        "{ran:?}"
    );
    assert_eq!(
        output(&session, "err(side.echo, text='x')").await,
        "refused stopped stopped None\n"
    );
    let QuiesceOutcome::Uncontrolled { why } =
        mcp.quiesce("side", Quiesce::Pause, Duration::ZERO).await
    else {
        panic!("a stopped service is not paused")
    };
    assert!(why.contains("was stopped"), "{why}");
    assert!(matches!(
        mcp.quiesce("side", Quiesce::Stop, Duration::ZERO).await,
        QuiesceOutcome::Quiesced { .. }
    ));
    let QuiesceOutcome::Uncontrolled { why } =
        mcp.quiesce("cmd", Quiesce::Pause, Duration::ZERO).await
    else {
        panic!("a command is uncontrolled")
    };
    assert!(why.contains("\"cmd\"") && why.contains("primary"), "{why}");
    let QuiesceOutcome::Uncontrolled { why } =
        mcp.quiesce("stuck", Quiesce::Pause, Duration::ZERO).await
    else {
        panic!("a failed freeze is uncontrolled")
    };
    assert!(why.contains("the freezer is broken"), "{why}");
    let report = stop(session).await;
    // The stop's unknown call ended before the close, so `services`, which lists what the close
    // ended, does not hold it; `mcp_uncertain` does, its server having had it, and so the verdict
    // is not clean.
    assert!(report.services.is_empty(), "{report:?}");
    let uncertain: Vec<_> = report
        .mcp_uncertain
        .iter()
        .map(|call| (call.tool.as_str(), call.failure, call.reason))
        .collect();
    assert_eq!(
        uncertain,
        [(
            "slow",
            Some(McpFailureKind::Canceled),
            Some(super::event::ServiceReason::Stopped)
        )]
    );
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    let events = taken(&mut events);
    let cancels = of_kind(&events, "service.call.cancel.sent");
    assert_eq!(cancels[0]["reason"], "stopped");
    let ended: Vec<Value> = of_kind(&events, "service.call.ended")
        .into_iter()
        .filter(|ended| ended["method"] == "slow")
        .collect();
    assert_eq!(
        (&ended[0]["outcome"], &ended[0]["reason"]),
        (&Value::from("unknown"), &Value::from("stopped"))
    );
}

/// A pause the owner gives up on part-way -- its call still settling -- runs to its end all the
/// same, so the resume after it finds what it did and undoes it: the next call works.
#[tokio::test]
async fn a_pause_given_up_on_is_resumed() {
    let fakes = Fakes::default();
    let (session, _events) = placed(
        &fakes,
        builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let mcp = session.mcp().clone();
    let mut sleeping = session.interpreter().submit("side.sleep(secs=1)").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pause = mcp.quiesce("side", Quiesce::Pause, Duration::from_secs(3));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), pause)
            .await
            .is_err(),
        "given up on while its call settles"
    );
    within(sleeping.outcome()).await;
    assert_eq!(mcp.resume("side").await, Ok(vec!["side".to_string()]));
    assert_eq!(
        output(&session, "print(side.echo(text='back').content[0].text)").await,
        "back\n"
    );
    stop(session).await;
}

/// A stop the owner gives up on part-way -- its container still stopping -- runs to its end all
/// the same: the service is stopped, and a resume says so rather than reopening it.
#[tokio::test]
async fn a_stop_given_up_on_still_stops() {
    let fakes = Fakes::default();
    fakes.of("mcp-side").slow_stop(Duration::from_millis(500));
    let (session, _events) = placed(
        &fakes,
        builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let mcp = session.mcp().clone();
    let mut slow = session.interpreter().submit("err(side.slow)").unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopping = mcp.quiesce("side", Quiesce::Stop, Duration::ZERO);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), stopping)
            .await
            .is_err(),
        "given up on while its container stops"
    );
    within(slow.outcome()).await;
    let resumed = mcp.resume("side").await;
    assert!(
        resumed
            .as_ref()
            .is_err_and(|why| why.contains("was stopped")),
        "{resumed:?}"
    );
    assert_eq!(fakes.of("mcp-side").stops(), 1);
    let report = stop(session).await;
    // The call the stop cut off may have reached its server.
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
}

/// A pause given up on before the step it spawned has run still comes before a resume asked
/// after it: the resume waits for it and undoes it, rather than finding the container running
/// and leaving the pause to freeze it once the resume has said it resumed.
///
/// On a worker, a task spawned from a task runs before the ones spawned earlier: the body runs
/// as one, so the resume's step is the one that would run first.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_resume_waits_for_a_pause_given_up_on() {
    let body = tokio::spawn(async {
        let fakes = Fakes::default();
        let (session, _events) = placed(
            &fakes,
            builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
        )
        .await;
        let mcp = session.mcp().clone();
        tokio::select! {
            biased;
            _ = mcp.quiesce("side", Quiesce::Pause, Duration::from_secs(1)) => {
                panic!("the pause's step had not run")
            }
            () = std::future::ready(()) => {}
        }
        assert_eq!(mcp.resume("side").await, Ok(vec!["side".to_string()]));
        assert_eq!(
            output(&session, "print(side.echo(text='back').content[0].text)").await,
            "back\n"
        );
        assert!(!fakes.of("mcp-side").frozen.load(Ordering::SeqCst));
        stop(session).await;
    });
    if let Err(e) = body.await {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// A sidecar that runs one server starts again with it: a failed attempt's container is
/// stopped before the next attempt makes another, so nothing the first left running runs
/// beside the second.
#[tokio::test]
async fn a_retry_starts_its_sidecar_again() {
    let fakes = Fakes::default();
    fakes.of("mcp-side").script("side", [Behavior::Hang]);
    let (session, _events) = placed(
        &fakes,
        builder().mcp("side", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let attempts = match outcome(&session, "side") {
        McpServiceOutcome::Ready { attempts, .. } => attempts,
        other => panic!("{other:?}"),
    };
    assert_eq!(attempts, 2);
    assert_eq!(
        fakes.of("mcp-side").stops(),
        1,
        "stopped between its attempts"
    );
    stop(session).await;
}

/// A sidecar none of whose servers started is stopped once the start ends, not left running
/// until the close: no writer lists it, and nothing could quiesce it meanwhile.
#[tokio::test]
async fn a_sidecar_that_serves_nothing_is_stopped_at_once() {
    let fakes = Fakes::default();
    let tools = SidecarSpec::from_image("tools", "tools-image")
        .with_server("gh", vec!["gh-server".to_string()])
        .with_server("grep", vec!["grep-server".to_string()]);
    fakes
        .of("tools")
        .script("gh", [Behavior::Exit(127, "gh: not found")]);
    fakes.of("tools").script("grep", [Behavior::Refuse]);
    let configured = ConfiguredMcp {
        sidecars: vec![ConfiguredSidecar {
            spec: tools,
            failed: None,
        }],
        ..ConfiguredMcp::default()
    };
    let (session, _events) = placed(
        &fakes,
        builder().mcp_configured(configured, Labels::default()),
    )
    .await;
    assert!(session.mcp().writers().is_empty());
    assert_eq!(fakes.of("tools").stops(), 1, "stopped before the close");
    let report = stop(session).await;
    assert!(
        report.mcp_services.iter().all(|row| row.proven),
        "{report:?}"
    );
}

/// A sidecar none of whose servers started, and that would not stop once the start ended, may
/// still run what their attempts left: its servers stay among the writers, and quiescing one says
/// why it cannot be.
#[tokio::test]
async fn a_sidecar_that_would_not_stop_stays_a_writer() {
    let fakes = Fakes::default();
    let data = tempfile::tempdir().expect("tempdir");
    let tools = SidecarSpec::from_image("tools", "tools-image")
        .with_mount(MountSpec::new(
            data.path(),
            "/data",
            crate::config::MountAccess::ReadWrite,
        ))
        .with_server("gh", vec!["gh-server".to_string()])
        .with_server("grep", vec!["grep-server".to_string()]);
    fakes.of("tools").script("gh", [Behavior::Refuse]);
    fakes.of("tools").script("grep", [Behavior::Refuse]);
    fakes.of("tools").fail_stop("podman stop timed out");
    let configured = ConfiguredMcp {
        sidecars: vec![ConfiguredSidecar {
            spec: tools,
            failed: None,
        }],
        ..ConfiguredMcp::default()
    };
    let (session, _events) = placed(
        &fakes,
        builder().mcp_configured(configured, Labels::default()),
    )
    .await;
    let writes = McpWrites::Paths(vec![data.path().to_path_buf()]);
    let writers: Vec<(String, McpWrites)> = session
        .mcp()
        .writers()
        .into_iter()
        .map(|writer| (writer.service, writer.writes))
        .collect();
    assert_eq!(
        writers,
        [
            ("gh".to_string(), writes.clone()),
            ("grep".to_string(), writes)
        ]
    );
    let paused = session
        .mcp()
        .quiesce("gh", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert!(
        matches!(&paused, QuiesceOutcome::Uncontrolled { why } if why.contains("not proven")),
        "{paused:?}"
    );
    stop(session).await;
}

/// A command that did not start may have left something running beside the interpreter: it
/// stays among the writers, and quiescing it says the primary cannot be frozen apart. One never
/// launched ran nothing, and is not listed.
#[tokio::test]
async fn a_command_that_did_not_start_stays_a_writer() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("refused", [Behavior::Refuse]);
    let credential = EnvValue::from_raw("${A}".to_string());
    let (session, _events) = placed(
        &fakes,
        builder()
            .mcp("refused", McpServiceSpec::command(["b"]))
            .mcp(
                "unlaunched",
                McpServiceSpec::command(["c"]).with_env("TOKEN", credential),
            ),
    )
    .await;
    let writers: Vec<(String, McpWrites)> = session
        .mcp()
        .writers()
        .into_iter()
        .map(|writer| (writer.service, writer.writes))
        .collect();
    assert_eq!(
        writers,
        [("refused".to_string(), McpWrites::PrimaryFilesystem)]
    );
    let paused = session
        .mcp()
        .quiesce("refused", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert!(
        matches!(&paused, QuiesceOutcome::Uncontrolled { why } if why.contains("primary container")),
        "{paused:?}"
    );
    stop(session).await;
}

/// A read of the primary image's label that the engine never answers holds nothing: an
/// interpreter that failed gives the servers up and the read with them, and with nothing
/// given up the read and the servers' starts share the one connect budget.
#[tokio::test]
async fn a_label_read_that_never_answers_is_given_up() {
    let budget = Duration::from_secs(1);
    let builder = builder()
        .mcp("hung", McpServiceSpec::command(["hung-server"]))
        .mcp_policy(policy(Duration::from_millis(300), budget));
    let events = crate::events::Events::off();
    let place = |_: &HostPlan| -> Arc<dyn Placement> {
        let fake = Fake::default();
        fake.script("hung", vec![Behavior::Hang; 10]);
        Arc::new(fake)
    };
    let given_up = CancellationToken::new();
    given_up.cancel();
    for cancel in [given_up, CancellationToken::new()] {
        let inputs = builder.mcp_inputs(Vec::new(), &events);
        let started = Instant::now();
        let start = inputs.start(None, std::future::pending(), None, place, cancel);
        tokio::time::timeout(Duration::from_secs(5), start)
            .await
            .expect("the read is given up");
        let took = started.elapsed();
        assert!(took < budget + budget / 2, "{took:?}");
    }
}

/// Services a configuration's block shares a container among are quiesced together.
#[tokio::test]
async fn siblings_in_one_container_are_quiesced_together() {
    let fakes = Fakes::default();
    let tools = SidecarSpec::from_image("tools", "tools-image")
        .with_server("gh", vec!["gh-server".to_string()])
        .with_server("grep", vec!["grep-server".to_string()]);
    let configured = ConfiguredMcp {
        sidecars: vec![ConfiguredSidecar {
            spec: tools,
            failed: None,
        }],
        ..ConfiguredMcp::default()
    };
    let (session, _events) = placed(
        &fakes,
        builder().mcp_configured(configured, Labels::default()),
    )
    .await;
    let mcp = session.mcp().clone();
    let paused = mcp
        .quiesce("gh", Quiesce::Pause, Duration::from_secs(1))
        .await;
    assert_eq!(
        paused,
        QuiesceOutcome::Quiesced {
            how: Quiesce::Pause,
            services: vec!["gh".into(), "grep".into()]
        }
    );
    assert_eq!(
        output(&session, "err(grep.echo, text='x')").await,
        "refused paused paused None\n"
    );
    assert_eq!(fakes.of("tools").freezes(), 1);
    stop(session).await;
    assert_eq!(
        fakes.of("tools").thaws(),
        1,
        "resumed before it was stopped"
    );
}

/// At the close each ready service's transport is closed and its container stopped, a paused
/// one thawed first; the report lists each, and a container that would not stop makes it not
/// proven stopped.
#[tokio::test]
async fn the_close_stops_every_service() {
    let fakes = Fakes::default();
    fakes.of("mcp-sticky").fail_stop("podman stop timed out");
    fakes
        .of(PRIMARY)
        .script("broken", [Behavior::Exit(127, "not found")]);
    let (session, _events) = placed(
        &fakes,
        builder()
            .mcp("side", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp("sticky", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp("cmd", McpServiceSpec::command(["a"]))
            .mcp("broken", McpServiceSpec::command(["b"])),
    )
    .await;
    let mcp = session.mcp().clone();
    mcp.quiesce("side", Quiesce::Pause, Duration::from_secs(1))
        .await;
    let report = stop(session).await;
    assert_eq!(fakes.of("mcp-side").thaws(), 1, "thawed before its stop");
    assert!(
        fakes
            .of("mcp-side")
            .stopped
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    let rows: Vec<(String, McpStopHow, bool)> = report
        .mcp_services
        .iter()
        .map(|row| (row.service.clone(), row.how, row.proven))
        .collect();
    assert_eq!(
        rows,
        [
            ("side".into(), McpStopHow::Stopped, true),
            ("sticky".into(), McpStopHow::Stopped, false),
            ("cmd".into(), McpStopHow::WithPrimary, true),
            ("broken".into(), McpStopHow::NotStarted, true),
        ]
    );
    let Stopped::NotProven { reason } = &report.stopped else {
        panic!("{:?}", report.stopped)
    };
    assert!(reason.contains("podman stop timed out"), "{reason}");
    assert_eq!(report.verdict(), Verdict::NotProvenStopped);
    assert_eq!(
        report
            .mcp_services
            .iter()
            .find(|row| row.service == "cmd")
            .map(|row| row.placement),
        Some(McpPlacementKind::Command)
    );
}

/// A stop that was not proven is tried again at the close, and a container that still will not
/// stop leaves the session not proven stopped.
#[tokio::test]
async fn an_unproven_stop_is_tried_again_at_the_close() {
    let fakes = Fakes::default();
    fakes.of("mcp-sticky").fail_stop("podman stop timed out");
    let (session, _events) = placed(
        &fakes,
        builder().mcp("sticky", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let QuiesceOutcome::Uncontrolled { why } = session
        .mcp()
        .quiesce("sticky", Quiesce::Stop, Duration::ZERO)
        .await
    else {
        panic!("a stop that failed is uncontrolled")
    };
    assert!(why.contains("podman stop timed out"), "{why}");
    let report = stop(session).await;
    assert_eq!(
        fakes.of("mcp-sticky").stops(),
        2,
        "tried again at the close"
    );
    assert_eq!(report.verdict(), Verdict::NotProvenStopped, "{report:?}");
    let Stopped::NotProven { reason } = &report.stopped else {
        panic!("{:?}", report.stopped)
    };
    assert!(reason.contains("podman stop timed out"), "{reason}");
    let row = &report.mcp_services[0];
    assert_eq!((row.how, row.proven), (McpStopHow::Stopped, false));
}

/// A server that never started may still have had a container, and its row is as proven as
/// that container's stop.
#[tokio::test]
async fn a_failed_start_is_as_proven_as_its_containers_stop() {
    let fakes = Fakes::default();
    fakes
        .of("mcp-broken")
        .script("broken", [Behavior::Exit(127, "not found")]);
    fakes.of("mcp-broken").fail_stop("podman stop timed out");
    let (session, _events) = placed(
        &fakes,
        builder().mcp("broken", McpServiceSpec::sidecar(sidecar(&[]))),
    )
    .await;
    let report = stop(session).await;
    let row = &report.mcp_services[0];
    assert_eq!(
        (row.how, row.proven),
        (McpStopHow::NotStarted, false),
        "{report:?}"
    );
    assert_eq!(report.verdict(), Verdict::NotProvenStopped);
}

/// A server in the session's container is stopped as that container is: its stop is said, not
/// proven, once the container's stop is not -- and so is one that did not start, which its
/// attempts ran there. One never launched ran nothing.
#[tokio::test]
async fn a_command_is_stopped_with_the_primary() {
    let fakes = Fakes::default();
    fakes.of(PRIMARY).script("refused", [Behavior::Refuse]);
    let credential = EnvValue::from_raw("${A}".to_string());
    let (session, mut events) = placed(
        &fakes,
        builder()
            .mcp("cmd", McpServiceSpec::command(["a"]))
            .mcp("refused", McpServiceSpec::command(["b"]))
            .mcp(
                "unlaunched",
                McpServiceSpec::command(["c"]).with_env("TOKEN", credential),
            )
            .fail_stop("the container did not stop"),
    )
    .await;
    assert!(matches!(
        outcome(&session, "unlaunched"),
        McpServiceOutcome::Misconfigured(_)
    ));
    let report = stop(session).await;
    let rows: Vec<(&str, McpStopHow, bool)> = report
        .mcp_services
        .iter()
        .map(|row| (row.service.as_str(), row.how, row.proven))
        .collect();
    assert_eq!(
        rows,
        [
            ("cmd", McpStopHow::WithPrimary, false),
            ("refused", McpStopHow::NotStarted, false),
            ("unlaunched", McpStopHow::NotStarted, true),
        ]
    );
    let events = taken(&mut events);
    let stopped = of_service(&events, "mcp.service.stopped", "cmd");
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0]["proven"], false);
}

/// A live object is not injected under an MCP service's name, whether or not it started.
#[tokio::test]
async fn a_live_object_takes_no_mcp_services_name() {
    let fakes = Fakes::default();
    fakes
        .of(PRIMARY)
        .script("broken", [Behavior::Exit(127, "not found")]);
    let (session, _events) = placed(
        &fakes,
        builder()
            .mcp("side", McpServiceSpec::sidecar(sidecar(&[])))
            .mcp("broken", McpServiceSpec::command(["b"])),
    )
    .await;
    for name in ["side", "broken"] {
        let brief = Brief {
            name: name.to_string(),
            description: String::new(),
            type_name: "__main__.Notes".to_string(),
            serialize: false,
        };
        let refused = session.inject(brief).await.unwrap_err();
        assert!(refused.contains("already binds"), "{refused}");
    }
    stop(session).await;
}

/// A declaration's header that names an identity header is refused, case aside.
#[test]
fn an_identity_header_is_not_a_declarations() {
    let identity = vec!["X-Fence".to_string()];
    let named = super::mcp::names_identity_header(
        &["Authorization".to_string(), "x-fence".to_string()],
        &identity,
    );
    assert_eq!(named.as_deref(), Some("x-fence"));
    assert_eq!(
        super::mcp::names_identity_header(&["Accept".to_string()], &identity),
        None
    );
}

/// A session that sets no policy starts its servers within the configuration's budget.
#[test]
fn the_configured_budget_is_the_default_policys() {
    let mut config = config();
    config.mcp_connect_budget_secs = Some(7);
    let builder = SessionBuilder::new(config, Some("coding"), None);
    assert_eq!(
        builder.mcp_policy_or_default().connect_budget,
        Duration::from_secs(7)
    );
    let chosen =
        builder.mcp_policy(McpPolicy::default().with_connect_budget(Duration::from_secs(9)));
    assert_eq!(
        chosen.mcp_policy_or_default().connect_budget,
        Duration::from_secs(9),
        "the owner's policy wins"
    );
}
