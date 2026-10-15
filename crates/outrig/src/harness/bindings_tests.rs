//! A session's bindings, end to end on the host: declared in config files or given through
//! `bind`, approved, installed from local wheels, started before the interpreter, described to
//! it, and stopped with the session. The e2e module at the end runs them beside a container.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;

use super::event::{Event, HostedOutcome, HostedReason, Payload, Received, Subscription};
use super::{
    Approval, ApprovalRequest, DEFAULT_DRAIN, HostedCall, Session, SessionBuilder, SessionError,
    ShutdownReport, Stopped, Verdict,
};
use crate::agent::mock_http::MODEL;
use crate::config::{Binding, Config, ConfigValidationError, FactoryValue};
use crate::error::OutrigError;
use crate::python::host::Outcome;
use crate::python::testing::{Flag, group_gone, pip_env, wait_group_gone, wheel_links, within};

const VAR: &str = "OUTRIG_TEST_BINDINGS_KEY";

/// A repository and a global config beside it, with a provider, a model and a `coding` agent,
/// and the directories a session's packages and approvals go in.
struct Bed {
    _root: tempfile::TempDir,
    repo: PathBuf,
    global: PathBuf,
    packages: PathBuf,
    approvals: PathBuf,
    records: PathBuf,
}

impl Bed {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let path = |name: &str| {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).expect("a directory");
            dir
        };
        let (repo, home, packages, approvals, records) = (
            path("repo"),
            path("home"),
            path("packages"),
            path("approvals"),
            path("records"),
        );
        std::fs::create_dir_all(repo.join(".agents/outrig")).expect("the repo config's directory");
        Self {
            global: home.join("config.toml"),
            _root: root,
            repo,
            packages,
            approvals,
            records,
        }
    }

    /// Write the global config, holding `global`'s bindings, and the repository's, holding
    /// `repo`'s.
    fn write(&self, global: &str, repo: &str) {
        std::fs::write(
            &self.global,
            format!(
                r#"
default-model = "sonnet"

[providers.claude]
style    = "anthropic"
base-url = "http://127.0.0.1:9"
api-key  = "${{{VAR}}}"

[models.sonnet]
provider   = "claude"
identifier = "{MODEL}"

[agents.coding]

{global}
"#
            ),
        )
        .expect("write the global config");
        std::fs::write(self.repo.join(".agents/outrig/config.toml"), repo)
            .expect("write the repo config");
    }

    fn load(&self) -> Config {
        Config::load(&self.repo, Some(&self.global)).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Where the recorder named `name` writes.
    fn record(&self, name: &str) -> PathBuf {
        self.records.join(format!("{name}.json"))
    }

    /// What the recorder named `name` wrote.
    fn recorded(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(self.record(name))
            .unwrap_or_else(|e| panic!("the binding {name} recorded nothing: {e}"));
        serde_json::from_str(&text).expect("a record")
    }

    /// A session over `cfg` in this repository, with its packages and approvals here.
    fn builder(&self, cfg: Config) -> SessionBuilder {
        SessionBuilder::new(cfg, Some("coding"), None)
            .secrets(|var: &str| (var == VAR).then(|| "sk-test".to_string()))
            .repo_root(&self.repo)
            .binding_places(&self.packages, pip_env(wheels()), &self.approvals)
    }

    /// `[bindings.<name>]`, a recorder of `verfix` `version` called with `{ path = "." }` and
    /// writing here, described as `description`.
    fn recorder(&self, name: &str, version: u8, description: &str) -> String {
        format!(
            r#"
[bindings.{name}]
description = "{description}"
requires    = ["recorder", "verfix=={version}.0"]
factory     = "recorder:make"
args        = [{{ path = "." }}]
kwargs      = {{ out = {out:?} }}
"#,
            out = self.record(name).to_str().expect("a UTF-8 path"),
        )
    }

    /// A recorder like [`Bed::recorder`]'s, for `bind`.
    fn bound(&self, name: &str, version: u8) -> Binding {
        let mut binding = Binding::new("recorder:make", format!("{name}, given through bind"));
        binding.requires = vec!["recorder".into(), format!("verfix=={version}.0")];
        binding.args = vec![FactoryValue::path(".")];
        binding.kwargs.insert(
            "out".into(),
            FactoryValue::String(self.record(name).to_str().expect("UTF-8").into()),
        );
        binding
    }
}

/// The test wheels' directory, built off the test's runtime: building them unpacks the payload
/// on a runtime of its own, which cannot start inside another.
fn wheels() -> &'static Path {
    std::thread::scope(|scope| scope.spawn(wheel_links).join().expect("the wheels build"))
}

/// An approver that answers `answer` and counts the requests it is shown.
fn counting(answer: Approval) -> (impl Fn(&ApprovalRequest) -> Approval, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let approver = move |_: &ApprovalRequest| {
        counter.fetch_add(1, Ordering::SeqCst);
        answer
    };
    (approver, asked)
}

async fn start(builder: SessionBuilder) -> Session {
    within(builder.start_on_host())
        .await
        .unwrap_or_else(|e| panic!("the session did not start: {e}"))
}

async fn stop(session: Session) -> ShutdownReport {
    within(session.shutdown(DEFAULT_DRAIN)).await
}

/// What `source` printed, run in `session`'s primary kernel.
async fn output(session: &Session, source: &str) -> String {
    let mut execution = session
        .interpreter()
        .submit(source)
        .unwrap_or_else(|e| panic!("{e}"));
    match within(execution.outcome()).await {
        Outcome::Ok(report) => report.output,
        other => panic!("{source:?} did not run: {other:?}"),
    }
}

/// The bindings' names, as `runtime.bindings` lists them.
async fn listed(session: &Session) -> String {
    output(session, "print(list(runtime.bindings))").await
}

/// Each `binding.refused` the subscription holds, as (binding, reason).
fn refusals(events: &mut Subscription) -> Vec<(String, String)> {
    let mut refused = Vec::new();
    while let Ok(received) = events.try_recv() {
        if let Received::Event(event) = received
            && let Payload::BindingRefused {
                binding, reason, ..
            } = &event.payload
        {
            refused.push((binding.clone(), reason.clone()));
        }
    }
    refused
}

/// The process `pid` a recorder wrote down, which leads its binding's group.
fn pid_of(pid: &Value) -> Pid {
    Pid::from_raw(pid.as_i64().expect("a pid") as i32)
}

/// `[bindings.fx]`: the hosted-call fixture, installed from the test wheels.
const FIXTURE: &str = r#"
[bindings.fx]
description = "the fixture"
requires    = ["outrig_fixture"]
factory     = "outrig_fixture:make"
"#;

/// A session holding [`FIXTURE`], its events subscribed to, and the bindings process's pid.
async fn with_fixture(bed: &Bed) -> (Session, Subscription, Pid) {
    bed.write(FIXTURE, "");
    let mut builder = bed.builder(bed.load());
    let events = builder.subscribe();
    let session = start(builder).await;
    let pid = session.bindings().pid().expect("the bindings process");
    (session, events, pid)
}

/// Wait, without holding the runtime, until `flag` is raised.
async fn raised(flag: &Flag) {
    within(async {
        while !flag.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

/// The events the subscription holds.
fn taken(events: &mut Subscription) -> Vec<Arc<Event>> {
    let mut held = Vec::new();
    while let Ok(received) = events.try_recv() {
        if let Received::Event(event) = received {
            held.push(event);
        }
    }
    held
}

/// The hosted calls -- `call` requests -- the report holds, with their outcomes and reasons.
fn calls_of(report: &ShutdownReport, events: &[Arc<Event>]) -> Vec<HostedCall> {
    let calls: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::HostedReceived {
                request,
                operation: super::event::Operation::Call,
                ..
            } => Some(*request),
            _ => None,
        })
        .collect();
    report
        .hosted
        .iter()
        .filter(|call| calls.contains(&call.request))
        .cloned()
        .collect()
}

/// The text of a session that failed to start.
fn failed(result: Result<Session, SessionError>) -> String {
    match result {
        Ok(_) => panic!("the session started"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn a_name_the_interpreter_holds_is_refused_by_validation() {
    for (name, taken) in [
        ("runtime", true),
        ("asyncio", true),
        ("outrig", true),
        ("__builtins__", true),
        ("class", false),
        ("2x", false),
        ("re-po", false),
    ] {
        let toml = format!("[bindings.{name:?}]\ndescription = \"d\"\nfactory = \"m:f\"\n");
        let err = Config::load_from_str(&toml)
            .expect("parses")
            .validate(None)
            .expect_err(name);
        let OutrigError::ConfigValidation(err) = err else {
            panic!("{err}");
        };
        match err {
            ConfigValidationError::BindingNameTaken { .. } if taken => {}
            ConfigValidationError::BindingName { .. } if !taken => {}
            other => panic!("{name}: {other}"),
        }
        assert!(err_text(&toml).contains(name), "{}", err_text(&toml));
    }
}

fn err_text(toml: &str) -> String {
    Config::load_from_str(toml)
        .expect("parses")
        .validate(None)
        .expect_err("refused")
        .to_string()
}

#[tokio::test]
async fn a_name_bind_cannot_use_fails_the_start() {
    let bed = Bed::new();
    bed.write(&bed.recorder("repo", 1, "global"), "");
    let cfg = bed.load();
    for name in ["runtime", "asyncio", "outrig"] {
        let text = failed(
            bed.builder(cfg.clone())
                .bind(name, bed.bound(name, 1))
                .start_on_host()
                .await,
        );
        assert!(text.contains(&format!("{name:?}")), "{text}");
    }
    let builder = bed.builder(cfg.clone()).bind("repo", bed.bound("repo", 2));
    let text = builder.check().expect_err("a repeated name").to_string();
    assert!(
        text.contains("`repo` given through `bind` is declared in"),
        "{text}"
    );
    let text = failed(builder.start_on_host().await);
    assert!(text.contains("`repo` given through `bind`"), "{text}");
    assert!(!bed.record("repo").exists(), "nothing started");
}

#[tokio::test]
async fn a_repo_binding_replacing_a_global_one_is_no_collision_and_is_asked_about() {
    let bed = Bed::new();
    bed.write(
        &bed.recorder("repo", 1, "the global one"),
        &bed.recorder("repo", 2, "the repository's own"),
    );
    let (approver, asked) = counting(Approval::Refuse);
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let shown = Arc::clone(&seen);
    let mut builder = bed
        .builder(bed.load())
        .approver(move |request: &ApprovalRequest| {
            *shown.lock().unwrap() = request.declaration.clone();
            approver(request)
        });
    let mut events = builder.subscribe();
    let session = start(builder).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(
        seen.lock().unwrap().contains("the repository's own"),
        "{}",
        seen.lock().unwrap()
    );
    assert_eq!(listed(&session).await, "[]\n");
    assert!(
        !bed.record("repo").exists(),
        "no binding named repo started"
    );
    let refused = refusals(&mut events);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, "repo");
    stop(session).await;
}

#[tokio::test]
async fn an_unapproved_repo_binding_does_not_start_and_says_why() {
    let bed = Bed::new();
    bed.write("", &bed.recorder("rec", 1, "the repository's"));
    for (answer, why) in [
        (
            Some(Approval::Refuse),
            "the operator did not approve its declaration",
        ),
        (
            Some(Approval::CannotAsk),
            "nobody could be asked to approve its declaration",
        ),
        (None, "the session has no approver"),
    ] {
        let mut builder = bed.builder(bed.load());
        if let Some(answer) = answer {
            builder = builder.approver(move |_: &ApprovalRequest| answer);
        }
        let mut events = builder.subscribe();
        let session = start(builder).await;
        assert_eq!(listed(&session).await, "[]\n");
        let refused = refusals(&mut events);
        assert_eq!(refused.len(), 1, "{refused:?}");
        assert_eq!(refused[0].0, "rec");
        assert!(refused[0].1.contains(why), "{}", refused[0].1);
        assert!(
            refused[0]
                .1
                .contains("move [bindings.rec] to the global config"),
            "{}",
            refused[0].1
        );
        assert!(!bed.record("rec").exists());
        stop(session).await;
    }
}

#[tokio::test]
async fn an_approval_is_remembered_and_an_edit_asks_again() {
    let bed = Bed::new();
    bed.write("", &bed.recorder("rec", 1, "first"));
    let (approver, asked) = counting(Approval::Approve);
    let approver = Arc::new(approver);
    let approve = || {
        let approver = Arc::clone(&approver);
        move |request: &ApprovalRequest| approver(request)
    };

    let session = start(bed.builder(bed.load()).approver(approve())).await;
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert_eq!(listed(&session).await, "['rec']\n");
    stop(session).await;

    let session = start(bed.builder(bed.load()).approver(approve())).await;
    assert_eq!(
        asked.load(Ordering::SeqCst),
        1,
        "approved, so not asked again"
    );
    assert_eq!(listed(&session).await, "['rec']\n");
    stop(session).await;

    bed.write("", &bed.recorder("rec", 1, "edited"));
    let session = start(bed.builder(bed.load()).approver(approve())).await;
    assert_eq!(asked.load(Ordering::SeqCst), 2, "an edit asks again");
    stop(session).await;

    // The same declaration in the global config asks nothing, and starts.
    bed.write(&bed.recorder("rec", 1, "the operator's"), "");
    let (never, asked) = counting(Approval::Refuse);
    let session = start(bed.builder(bed.load()).approver(never)).await;
    assert_eq!(asked.load(Ordering::SeqCst), 0);
    assert_eq!(listed(&session).await, "['rec']\n");
    stop(session).await;
}

#[tokio::test]
async fn a_relative_path_reaches_the_factory_as_the_repository_root() {
    let bed = Bed::new();
    bed.write(
        &bed.recorder("operator", 1, "global"),
        &bed.recorder("repo", 1, "repo"),
    );
    let session = start(
        bed.builder(bed.load())
            .approver(|_: &ApprovalRequest| Approval::Approve)
            .bind("given", bed.bound("given", 1)),
    )
    .await;
    let root = bed.repo.to_str().expect("UTF-8");
    for name in ["operator", "repo", "given"] {
        let record = bed.recorded(name);
        assert_eq!(record["args"], serde_json::json!([root]), "{name}");
        // A binding starts in the workspace's host directory, here the repository.
        assert_eq!(
            Path::new(record["cwd"].as_str().expect("a cwd")),
            bed.repo.canonicalize().expect("the repo"),
            "{name}"
        );
    }
    stop(session).await;
}

#[tokio::test]
async fn a_relative_repository_root_is_made_absolute_before_anything_keys_on_it() {
    let bed = Bed::new();
    bed.write("", &bed.recorder("repo", 1, "repo"));
    // The bed's repository, named relative to the test's directory.
    let cwd = std::env::current_dir().expect("the current directory");
    let relative: PathBuf = cwd
        .components()
        .skip(1)
        .map(|_| Path::new(".."))
        .collect::<PathBuf>()
        .join(bed.repo.strip_prefix("/").expect("an absolute repo"));
    let absolute = cwd.join(&relative);
    let shown = Arc::new(std::sync::Mutex::new(Vec::new()));
    let approver = {
        let shown = Arc::clone(&shown);
        move |request: &ApprovalRequest| {
            shown
                .lock()
                .expect("unpoisoned")
                .push(request.repo_root.clone());
            Approval::Approve
        }
    };
    let session = start(
        bed.builder(bed.load())
            .repo_root(&relative)
            .approver(approver),
    )
    .await;
    assert_eq!(
        *shown.lock().expect("unpoisoned"),
        std::slice::from_ref(&absolute)
    );
    let record = bed.recorded("repo");
    assert_eq!(
        record["args"],
        serde_json::json!([absolute.to_str().expect("UTF-8")])
    );
    stop(session).await;
}

#[tokio::test]
async fn a_binding_inherits_no_variable_the_config_names() {
    let secret = "OUTRIG_TEST_BINDING_SECRET";
    let bed = Bed::new();
    bed.write(&bed.recorder("rec", 1, "d"), "");
    // The config names the variable as a provider's key, as `${VAR}`.
    let text = std::fs::read_to_string(&bed.global)
        .expect("the global config")
        .replace(&format!("${{{VAR}}}"), &format!("${{{secret}}}"));
    std::fs::write(&bed.global, text).expect("rewrite it");
    let builder = |bed: &Bed| {
        SessionBuilder::new(bed.load(), Some("coding"), None)
            .secrets(move |var: &str| (var == secret).then(|| "sk-test".to_string()))
            .repo_root(&bed.repo)
            .binding_places(&bed.packages, pip_env(wheels()), &bed.approvals)
    };
    // SAFETY: edition 2024 marks `set_var` unsafe for its races with other threads reading the
    // environment; this variable is this test's alone, as the agent tests' keys are theirs.
    unsafe { std::env::set_var(secret, "sk-must-not-reach-a-binding") };
    let session = start(builder(&bed)).await;
    let env = &bed.recorded("rec")["env"];
    assert!(env.get(secret).is_none(), "{env}");
    assert!(env.get("PATH").is_some(), "the rest is inherited: {env}");
    stop(session).await;

    // An environment the owner gives is the whole of it, less what CPython sets for its own
    // locale.
    let session = start(builder(&bed).bindings_env([("ONLY", "this")])).await;
    unsafe { std::env::remove_var(secret) };
    let exact = &bed.recorded("rec")["env"];
    assert_eq!(exact["ONLY"], "this", "{exact}");
    assert!(
        exact.get("PATH").is_none() && exact.get(secret).is_none(),
        "{exact}"
    );
    stop(session).await;
}

#[tokio::test]
async fn a_factory_that_raises_fails_the_start_and_leaves_no_binding_running() {
    let bed = Bed::new();
    let failing = r#"
[bindings.zzz]
description = "fails"
requires    = ["recorder"]
factory     = "recorder:failing"
"#;
    bed.write(
        &format!("{}{failing}", bed.recorder("aaa", 1, "starts")),
        "",
    );
    let text = failed(bed.builder(bed.load()).start_on_host().await);
    assert!(text.contains("the binding `zzz` did not start"), "{text}");
    assert!(
        text.contains("ValueError: the recorder was told to fail"),
        "{text}"
    );
    wait_group_gone(pid_of(&bed.recorded("aaa")["pid"]), Duration::from_secs(10));
}

#[tokio::test]
async fn runtime_bindings_answers_without_asking_the_binding() {
    let bed = Bed::new();
    bed.write(&bed.recorder("rec", 1, "Records what it is given."), "");
    let session = start(bed.builder(bed.load())).await;
    let pid = session.bindings().pid().expect("the bindings run");
    kill(pid, Signal::SIGSTOP).expect("stop the binding");
    let said = output(
        &session,
        "b = runtime.bindings['rec']\nprint(list(runtime.bindings), b.name, b.description, b.type)",
    )
    .await;
    kill(pid, Signal::SIGCONT).expect("continue the binding");
    assert_eq!(
        said,
        "['rec'] rec Records what it is given. recorder.Recorded\n"
    );
    stop(session).await;
}

#[tokio::test]
async fn the_binding_order_decides_the_version_the_interpreter_imports() {
    let bed = Bed::new();
    bed.write(
        &bed.recorder("z", 1, "global"),
        &format!(
            "{}{}",
            bed.recorder("b", 3, "repo"),
            bed.recorder("a", 2, "repo")
        ),
    );
    let session = start(
        bed.builder(bed.load())
            .approver(|_: &ApprovalRequest| Approval::Approve)
            .bind("c", bed.bound("c", 4)),
    )
    .await;
    assert_eq!(listed(&session).await, "['z', 'a', 'b', 'c']\n");
    // One process holds every binding, and its `sys.path` has the same order: each imports the
    // global binding's version.
    for name in ["z", "a", "b", "c"] {
        assert_eq!(bed.recorded(name)["verfix"], "1.0", "{name}");
    }
    let packages = bed.packages.to_str().expect("UTF-8");
    let said = output(
        &session,
        &format!(
            "import sys, verfix\nprint(verfix.VERSION)\n\
             print(len([p for p in sys.path if p.startswith({packages:?})]))"
        ),
    )
    .await;
    assert_eq!(said, "1.0\n4\n", "the global binding's version comes first");
    stop(session).await;
}

#[tokio::test]
async fn a_binding_whose_process_died_is_stopped_with_its_group() {
    let bed = Bed::new();
    bed.write(
        &format!(
            r#"
[bindings.spawner]
description = "starts a sleep"
requires    = ["recorder"]
factory     = "recorder:spawning"
kwargs      = {{ out = {:?} }}
"#,
            bed.record("spawner").to_str().expect("UTF-8")
        ),
        "",
    );
    let mut builder = bed.builder(bed.load());
    let mut events = builder.subscribe();
    let session = start(builder).await;
    let record = bed.recorded("spawner");
    let (pid, child) = (pid_of(&record["pid"]), pid_of(&record["child"]));
    // The binding alone dies; what it started lives on in its group.
    kill(pid, Signal::SIGKILL).expect("kill the binding");
    assert!(!group_gone(pid), "the sleep is still in the group");
    // The stream records the exit, and how the process ended.
    let exited = within(async {
        loop {
            match events.recv().await {
                Some(Received::Event(event)) => {
                    if let Payload::BindingExited { cause, .. } = &event.payload {
                        return cause.clone();
                    }
                }
                Some(Received::Missed(_)) => {}
                None => panic!("the stream ended before the exit"),
            }
        }
    })
    .await;
    assert!(exited.contains("SIGKILL"), "{exited}");
    let report = stop(session).await;
    assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
    let stop_of = report
        .bindings
        .as_ref()
        .expect("the bindings process stopped");
    assert_eq!(stop_of.names, ["spawner"]);
    assert!(stop_of.group_empty, "{report:?}");
    assert!(
        stop_of
            .exited
            .as_deref()
            .is_some_and(|cause| cause.contains("SIGKILL")),
        "{report:?}"
    );
    assert!(group_gone(pid), "nothing is left of the group");
    assert_eq!(kill(child, None), Err(Errno::ESRCH), "the sleep is gone");
}

/// Agent code in a session reaches its binding through the binding's name, and each request is
/// recorded in the session's stream.
#[tokio::test]
async fn agent_code_reaches_its_binding_by_name() {
    let bed = Bed::new();
    let (session, mut events, _) = with_fixture(&bed).await;
    assert_eq!(output(&session, "print(fx.public)").await, "public value\n");
    let report = stop(session).await;
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
    let kinds: Vec<&str> = taken(&mut events)
        .iter()
        .map(|event| event.kind())
        .filter(|kind| kind.starts_with("hosted."))
        .collect();
    assert_eq!(
        kinds,
        ["hosted.received", "hosted.dispatched", "hosted.ended"]
    );
}

/// Shutdown during a slow hosted call kills the bindings process once the drain's deadline has
/// passed: the call is `unknown`, its binding killed at shutdown, in the report and in its
/// outcome event, and nothing is left of the process's group.
#[tokio::test]
async fn shutdown_during_a_slow_call_kills_it_and_reports_it_unknown() {
    let bed = Bed::new();
    let (session, mut events, pid) = with_fixture(&bed).await;
    let started = Flag::new();
    let _slow = session
        .interpreter()
        .submit(&format!("fx.record(60, 'slow', started={})", started.py()))
        .unwrap_or_else(|e| panic!("{e}"));
    raised(&started).await;
    let report = within(session.shutdown(Duration::from_millis(300))).await;
    let events = taken(&mut events);
    let calls = calls_of(&report, &events);
    assert_eq!(calls.len(), 1, "{report:?}");
    assert_eq!(
        (calls[0].outcome, calls[0].reason),
        (HostedOutcome::Unknown, Some(HostedReason::Killed))
    );
    assert!(
        events.iter().any(|event| matches!(&event.payload,
            Payload::HostedEnded { request, outcome: HostedOutcome::Unknown,
                reason: Some(HostedReason::Killed), .. } if *request == calls[0].request)),
        "no outcome event"
    );
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    assert!(
        group_gone(pid),
        "something of the bindings process's group is left"
    );
}

/// The helper `fx.escape` forked, killed with the test.
struct Escaped(Pid);

impl Drop for Escaped {
    fn drop(&mut self) {
        let _ = kill(self.0, Signal::SIGKILL);
    }
}

/// A helper the binding's library forked into a session of its own holds the bindings process's
/// output open past the stop. The shutdown waits for the relay a bounded time and then takes the
/// process as gone: the call in flight is `unknown`, its binding killed, in the report and in its
/// outcome event, and nothing hosted is recorded after the report.
#[tokio::test]
async fn an_output_held_past_the_stop_leaves_the_call_unknown() {
    let bed = Bed::new();
    let (session, mut events, _) = with_fixture(&bed).await;
    let pidfile = bed.records.join("escaped");
    let _call = session
        .interpreter()
        .submit(&format!(
            "fx.escape({:?}, 60)",
            pidfile.to_str().expect("a UTF-8 path")
        ))
        .unwrap_or_else(|e| panic!("{e}"));
    let escaped = within(async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pidfile) {
                return Escaped(Pid::from_raw(pid.parse().expect("a pid")));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let report = within(session.shutdown(Duration::from_millis(300))).await;
    assert!(
        kill(escaped.0, None).is_ok(),
        "the helper did not outlive the stop"
    );
    let events = taken(&mut events);
    let calls = calls_of(&report, &events);
    assert_eq!(calls.len(), 1, "{report:?}");
    assert_eq!(
        (calls[0].outcome, calls[0].reason),
        (HostedOutcome::Unknown, Some(HostedReason::Killed))
    );
    assert!(
        events.iter().any(|event| matches!(&event.payload,
            Payload::HostedEnded { request, outcome: HostedOutcome::Unknown,
                reason: Some(HostedReason::Killed), .. } if *request == calls[0].request)),
        "no outcome event"
    );
    assert_eq!(report.verdict(), Verdict::StoppedWithUnknown, "{report:?}");
    let reported = events
        .iter()
        .position(|event| matches!(event.payload, Payload::SessionReport { .. }))
        .expect("the report");
    assert!(
        !events[reported..]
            .iter()
            .any(|event| event.kind().starts_with("hosted.")),
        "a hosted event after the report"
    );
}

/// A call draining at the close calls back into the agent's code: the callback runs during the
/// drain, and the call ends `returned`.
#[tokio::test]
async fn a_draining_calls_callback_runs() {
    let bed = Bed::new();
    let (session, mut events, _) = with_fixture(&bed).await;
    let (started, called) = (Flag::new(), Flag::new());
    let _call = session
        .interpreter()
        .submit(&format!(
            "def back():\n    open({}, 'w').close()\n    return 'back'\n\
             print(fx.call_after(1, back, started={}))",
            called.py(),
            started.py()
        ))
        .unwrap_or_else(|e| panic!("{e}"));
    raised(&started).await;
    let report = stop(session).await;
    assert!(called.exists(), "the callback did not run");
    let events = taken(&mut events);
    let calls = calls_of(&report, &events);
    assert_eq!(calls.len(), 1, "{report:?}");
    assert_eq!(calls[0].outcome, HostedOutcome::Returned);
    assert!(
        events.iter().any(|event| matches!(&event.payload,
            Payload::HostedCallback { parent, .. } if *parent == calls[0].request)),
        "no callback event under the call"
    );
    assert_eq!(report.verdict(), Verdict::Clean, "{report:?}");
}

/// Callbacks that never return -- a function looping on the primary's thread, in a call made
/// there, and one looping on a worker, in a call made with `asyncio.to_thread` -- made by calls
/// draining at the close leave shutdown bounded. The interrupt the shutdown sends lands in the one
/// on the primary's thread, which answers its call with it, so that call ends `raised`; nothing
/// interrupts a worker, so the other call ends `unknown`, its binding killed. Nothing is left of
/// the bindings process's group.
#[tokio::test]
async fn a_callback_that_never_returns_leaves_shutdown_bounded() {
    let bed = Bed::new();
    let (session, mut events, pid) = with_fixture(&bed).await;
    let (a, b) = (Flag::new(), Flag::new());
    let _calls = session
        .interpreter()
        .submit(&format!(
            "import asyncio, os, time\n\
             def spin():\n\
             \x20   while True:\n\
             \x20       time.sleep(0.01)\n\
             worker = asyncio.create_task(asyncio.to_thread(\n\
             \x20   lambda: fx.call_after(0.2, spin, started={worker})))\n\
             while not os.path.exists({worker}):\n\
             \x20   await asyncio.sleep(0.01)\n\
             fx.call_after(0.2, spin, started={primary})",
            worker = a.py(),
            primary = b.py()
        ))
        .unwrap_or_else(|e| panic!("{e}"));
    raised(&a).await;
    raised(&b).await;
    // Both callbacks are running.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let deadline = Duration::from_secs(1);
    let begun = Instant::now();
    let report = within(session.shutdown(deadline)).await;
    // The terminating step's own limits: the interrupt's grace, the bindings' grace and the
    // group's kill, and the interpreter's exit.
    let bound = deadline + Duration::from_secs(20);
    assert!(
        begun.elapsed() < bound,
        "shutdown took {:?}",
        begun.elapsed()
    );
    let events = taken(&mut events);
    let calls = calls_of(&report, &events);
    assert_eq!(calls.len(), 2, "{report:?}");
    // Which call was passed which callback, by the flag each was given.
    let passed = |flag: &Flag| {
        let path = flag.path().to_str().expect("a UTF-8 path");
        events
            .iter()
            .find_map(|event| match &event.payload {
                Payload::HostedReceived {
                    request, keywords, ..
                } if keywords.get("started").is_some_and(|it| it.contains(path)) => Some(*request),
                _ => None,
            })
            .and_then(|request| calls.iter().find(|call| call.request == request))
            .map(|call| (call.outcome, call.reason))
    };
    assert_eq!(
        passed(&a),
        Some((HostedOutcome::Unknown, Some(HostedReason::Killed))),
        "{report:?}"
    );
    assert_eq!(
        passed(&b),
        Some((HostedOutcome::Raised, None)),
        "{report:?}"
    );
    assert!(
        group_gone(pid),
        "something of the bindings process's group is left"
    );
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::python::testing::{ALPINE, pull};
    use crate::{LaunchSpec, WorkspaceSpec};

    /// A session of `bed`'s config in an Alpine container whose workspace is the repository,
    /// declared at `/workspace` -- which a binding moves to its host path.
    async fn in_a_container(bed: &Bed, builder: SessionBuilder) -> Session {
        pull(ALPINE).await;
        let logs = bed.records.join("logs");
        std::fs::create_dir_all(&logs).expect("a log directory");
        let spec = LaunchSpec::from_image(ALPINE, BTreeMap::new(), logs)
            .with_workspace(WorkspaceSpec::new(bed.repo.clone(), "/workspace"));
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            builder.container(spec).start(),
        )
        .await
        .expect("the session starts in time")
        .unwrap_or_else(|e| panic!("the session did not start: {e}"))
    }

    #[tokio::test]
    async fn the_requirement_imports_in_the_container_from_its_read_only_directory() {
        let bed = Bed::new();
        bed.write(&bed.recorder("rec", 2, "d"), "");
        let session = in_a_container(&bed, bed.builder(bed.load())).await;
        let said = output(
            &session,
            "import os, recorder, verfix\n\
             print(verfix.VERSION, verfix.__file__.startswith('/outrig/bindings/'))\n\
             try:\n\
             \x20   open(os.path.join(os.path.dirname(verfix.__file__), 'x'), 'w')\n\
             except OSError as e:\n\
             \x20   print('read-only:', type(e).__name__)",
        )
        .await;
        assert_eq!(said, "2.0 True\nread-only: OSError\n");
        stop(session).await;
    }

    #[tokio::test]
    async fn the_binding_order_decides_the_version_the_container_imports() {
        let bed = Bed::new();
        bed.write(
            &bed.recorder("z", 1, "global"),
            &format!(
                "{}{}",
                bed.recorder("b", 3, "repo"),
                bed.recorder("a", 2, "repo")
            ),
        );
        let session = in_a_container(
            &bed,
            bed.builder(bed.load())
                .approver(|_: &ApprovalRequest| Approval::Approve)
                .bind("c", bed.bound("c", 4)),
        )
        .await;
        let said = output(
            &session,
            "import os, sys, verfix\n\
             dirs = [p for p in sys.path if p.startswith('/outrig/bindings/')]\n\
             print([open(os.path.join(p, 'verfix.py')).read().strip() for p in dirs])\n\
             print(verfix.VERSION)",
        )
        .await;
        assert_eq!(
            said,
            "[\"VERSION = '1.0'\", \"VERSION = '2.0'\", \"VERSION = '3.0'\", \
             \"VERSION = '4.0'\"]\n1.0\n"
        );
        // One process holds every binding, and imports what the container imports: the first
        // binding's version.
        for name in ["z", "a", "b", "c"] {
            assert_eq!(bed.recorded(name)["verfix"], "1.0", "{name}");
        }
        stop(session).await;
    }

    #[tokio::test]
    async fn a_path_names_one_file_in_the_container_and_on_the_host() {
        let bed = Bed::new();
        bed.write(&bed.recorder("rec", 1, "d"), "");
        let session = in_a_container(&bed, bed.builder(bed.load())).await;
        let said = output(
            &session,
            "import os\nopen('written.txt', 'w').write('from the container')\nprint(os.getcwd())",
        )
        .await;
        assert_eq!(said.trim_end(), bed.repo.to_str().expect("UTF-8"));
        assert_eq!(
            std::fs::read_to_string(bed.repo.join("written.txt")).expect("on the host"),
            "from the container"
        );
        // The binding runs in the same directory.
        assert_eq!(
            Path::new(bed.recorded("rec")["cwd"].as_str().expect("a cwd")),
            bed.repo.canonicalize().expect("the repo")
        );
        stop(session).await;
    }

    #[tokio::test]
    async fn nothing_a_binding_started_outlives_the_shutdown() {
        let bed = Bed::new();
        bed.write(
            &format!(
                "[bindings.spawner]\ndescription = \"starts a sleep\"\nrequires = [\"recorder\"]\n\
                 factory = \"recorder:spawning\"\nkwargs = {{ out = {:?} }}\n",
                bed.record("spawner").to_str().expect("UTF-8")
            ),
            "",
        );
        let session = in_a_container(&bed, bed.builder(bed.load())).await;
        let record = bed.recorded("spawner");
        let (pid, child) = (pid_of(&record["pid"]), pid_of(&record["child"]));
        let report = stop(session).await;
        assert_eq!(report.stopped, Stopped::Proven, "{report:?}");
        assert!(
            report
                .bindings
                .as_ref()
                .is_some_and(|stop| stop.group_empty),
            "{report:?}"
        );
        assert!(group_gone(pid));
        assert_eq!(kill(child, None), Err(Errno::ESRCH));
    }
}
