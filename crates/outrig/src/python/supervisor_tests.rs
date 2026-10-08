//! The binding supervisor against real processes: a binding's group ends with its owner --
//! killed with `kill -9`, or gone before the parent-death signal was set -- and not with a pool
//! thread's exit or a Ctrl-C; its stop escalates and proves its group empty; and its stdio
//! carries events and decision requests beside the frames.
//!
//! The owner's death needs an owner that is a process of its own: [`owner_process`] is this
//! test binary re-run on that one test, with `OUTRIG_TEST_OWNER_ROLE` saying what it does and
//! `OUTRIG_TEST_OWNER_DIR` where it reports, started in a process group of its own so that a
//! signal to that group reaches nothing else.

use std::ffi::OsString;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, kill, killpg, sigaction};
use nix::unistd::Pid;
use serde_json::json;

use super::relay::{Bind, Relay, TIMEOUT, fixture_dir, rpyc_dir};
use super::supervisor::{self, Probe, STOP_GRACE, Spec, group_members};
use super::testing::{Flag, capture, eventually, py, python};

const FACTORY: &str = "outrig_fixture:make";
const ROLE: &str = "OUTRIG_TEST_OWNER_ROLE";
const DIR: &str = "OUTRIG_TEST_OWNER_DIR";
/// The acceptance's bound on a binding's group being gone after its owner's death.
const GONE_WITHIN: Duration = Duration::from_secs(5);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime")
}

fn spec<'a>(factory: &'a str, env: Option<&'a [(OsString, OsString)]>) -> Spec<'a> {
    Spec {
        python: python(),
        rpyc_dir: rpyc_dir(),
        packages: fixture_dir(),
        factory,
        serialize: false,
        cwd: None,
        env,
        probe: None,
    }
}

/// Whether no process is left in `pgid`'s group: `ESRCH`, and nothing else.
fn group_gone(pgid: Pid) -> bool {
    kill(Pid::from_raw(-pgid.as_raw()), None) == Err(Errno::ESRCH)
}

/// Wait up to `within` for the group to be gone, failing with what is left in it.
fn wait_group_gone(pgid: Pid, within: Duration) {
    let deadline = Instant::now() + within;
    while !group_gone(pgid) {
        assert!(
            Instant::now() < deadline,
            "group {pgid} still holds {:?} after {within:?}",
            group_members(pgid)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` is a live process: in `/proc`, and not a zombie.
fn alive(pid: Pid) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

/// Python that binds `root` to `agent`'s proxy of binding `fx` and runs `body`.
fn with_root_in(agent: &str, body: &str) -> String {
    format!(
        "import __main__\nroot = __main__._kernels[{agent:?}].hosted('fx')\n{}",
        py(body)
    )
}

fn with_root(body: &str) -> String {
    with_root_in("primary", body)
}

// ---------------------------------------------------------------------------- the owner as a process

/// The owner as a process of its own: this test binary re-run on this one test, which does
/// nothing unless `ROLE` names a role. Each role starts its binding through the supervisor, as a
/// session does, reports into `DIR`, and then waits to be killed.
#[test]
#[ignore = "the owner helper, run by other tests as a process of its own"]
fn owner_process() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os(DIR).expect("the owner's directory"));
    match role.to_str().expect("a role") {
        "slow-call" => owner_slow_call(&dir),
        "race" => owner_race(&dir),
        "ctrl-c" => owner_ctrl_c(&dir),
        other => panic!("no role {other:?}"),
    }
}

/// A binding in a call that started `sleep 1000` as a grandchild; reports `pgid`.
fn owner_slow_call(dir: &Path) {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let started = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_root(&format!("root.sleep_child(1000, started={})", started.py())),
    );
    started.wait();
    report(dir, "pgid", &relay.binding_pid("fx").to_string());
    park();
}

/// A binding whose child sleeps between the fork and the setting of the parent-death signal,
/// and says what it found in `probe`; the test kills this process inside that window.
fn owner_race(dir: &Path) {
    let runtime = runtime();
    let mut spec = spec(FACTORY, None);
    spec.probe = Some(Probe {
        delay: Duration::from_secs(2),
        path: dir.join("probe"),
    });
    let _binding = runtime.block_on(supervisor::start(spec));
    park();
}

/// A binding, and an owner that survives SIGINT as the CLI does: reports `pgid`, waits for
/// `sent`, then makes a call and reports its output as `after`.
fn owner_ctrl_c(dir: &Path) {
    extern "C" fn nothing(_: nix::libc::c_int) {}
    // A handler, which exec resets, rather than SIG_IGN, which the binding would inherit.
    let action = SigAction::new(
        SigHandler::Handler(nothing),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    // SAFETY: the handler does nothing at all.
    unsafe { sigaction(Signal::SIGINT, &action) }.expect("install the SIGINT handler");
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    report(dir, "pgid", &relay.binding_pid("fx").to_string());
    let sent = dir.join("sent");
    eventually(
        || sent.exists().then_some(()),
        || "the test's `sent`".to_string(),
    );
    let out = relay.output(1, &with_root("print(root.method(1))"));
    report(dir, "after", &out);
    park();
}

/// Write `text` as `dir/name`, whole or not at all.
fn report(dir: &Path, name: &str, text: &str) {
    let staged = dir.join(format!(".{name}"));
    std::fs::write(&staged, text).expect("write the report");
    std::fs::rename(&staged, dir.join(name)).expect("publish the report");
}

fn park() -> ! {
    loop {
        std::thread::park();
    }
}

/// An owner running as a process of its own, in a process group of its own.
struct Owner {
    child: std::process::Child,
    dir: tempfile::TempDir,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
}

impl Owner {
    fn start(role: &str) -> Self {
        // libtest names each test's thread for it, so the helper's path is this module's.
        let test = std::thread::current()
            .name()
            .expect("libtest names each test's thread")
            .to_owned();
        let (module, _) = test.rsplit_once("::").expect("a test inside a module");
        let dir = tempfile::tempdir().expect("the owner's directory");
        let mut child = Command::new(std::env::current_exe().expect("this test binary"))
            .args([&format!("{module}::owner_process"), "--exact", "--ignored"])
            .env(ROLE, role)
            .env(DIR, dir.path())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the owner starts");
        let stdout = capture(child.stdout.take().expect("stdout is piped"));
        let stderr = capture(child.stderr.take().expect("stderr is piped"));
        Self {
            child,
            dir,
            stdout,
            stderr,
        }
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id() as i32)
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.path().join(name)).ok()
    }

    /// What the owner reported as `name`, within the relay's timeout: a role starts an
    /// interpreter before it reports.
    fn report(&mut self, name: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(text) = self.read(name) {
                return text;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "the owner exited ({status}) before reporting {name}\n{}",
                    self.said()
                );
            }
            assert!(
                Instant::now() < deadline,
                "no {name} from the owner within {TIMEOUT:?}\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn said(&self) -> String {
        format!(
            "owner stdout:\n{}\nowner stderr:\n{}",
            self.stdout.lock().expect("stdout lock"),
            self.stderr.lock().expect("stderr lock")
        )
    }

    /// The group of the owner's binding.
    fn pgid(&mut self) -> Pid {
        Pid::from_raw(self.report("pgid").trim().parse().expect("a pid"))
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.path().join(name), text).expect("write to the owner");
    }

    /// `kill -9` of the owner: the end nothing in it can respond to.
    fn kill(&mut self) {
        kill(self.pid(), Signal::SIGKILL).expect("kill the owner");
        let _ = self.child.wait();
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = killpg(self.pid(), Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------- the owner's death

#[test]
fn kill_9_of_the_owner_leaves_nothing_in_the_bindings_group() {
    let mut owner = Owner::start("slow-call");
    let pgid = owner.pgid();
    let before = group_members(pgid);
    assert!(
        before.iter().any(|member| member.pid == pgid)
            && before.iter().any(|member| member.comm == "sleep"),
        "the binding and the grandchild its call started: {before:?}"
    );
    owner.kill();
    wait_group_gone(pgid, GONE_WITHIN);
}

#[test]
fn an_owner_killed_before_the_signal_is_set_leaves_no_binding() {
    let mut owner = Owner::start("race");
    let forked = eventually(
        || owner.read("probe").filter(|text| text.contains('\n')),
        || "the probe's forked line".to_string(),
    );
    let pid: i32 = forked
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("forked "))
        .expect("`forked <pid>`")
        .trim()
        .parse()
        .expect("a pid");
    let pgid = Pid::from_raw(pid);
    assert!(owner.running(), "the owner lives through the fork");
    owner.kill();
    let verdict = eventually(
        || owner.read("probe").filter(|text| text.lines().count() >= 2),
        || "the probe's verdict".to_string(),
    );
    assert_eq!(verdict.lines().nth(1), Some("parent-changed"), "{verdict}");
    wait_group_gone(pgid, GONE_WITHIN);
}

#[test]
fn sigint_to_the_owners_group_leaves_the_binding_running() {
    let mut owner = Owner::start("ctrl-c");
    let pgid = owner.pgid();
    killpg(owner.pid(), Signal::SIGINT).expect("Ctrl-C to the owner's group");
    owner.write("sent", "");
    assert_eq!(owner.report("after"), "2\n");
    assert!(alive(pgid) && owner.running());
}

/// The parent-death signal names the thread that created the process. A binding started from a
/// tokio blocking-pool thread would be signaled when that thread exits after its idle timeout;
/// one started through the supervisor is not, because its thread lives as long as the process.
/// The control is a process started on the pool thread with the same signal.
#[test]
fn a_pool_threads_exit_is_not_a_parents_death() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .thread_keep_alive(Duration::from_millis(100))
        .enable_all()
        .build()
        .expect("a runtime");
    // Built here: the fixture paths are unpacked on a runtime of their own the first time they
    // are asked for, which cannot happen inside this one.
    let spec = spec(FACTORY, None);
    let (binding, mut control) = runtime.block_on(async {
        tokio::task::spawn_blocking(move || {
            let handle = tokio::runtime::Handle::current();
            let binding = handle
                .block_on(supervisor::start(spec))
                .expect("the binding starts");
            let mut control = Command::new(python());
            control
                .args(["-I", "-c", "import time; time.sleep(100)"])
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // SAFETY: two async-signal-safe calls between fork and exec.
            unsafe {
                control.pre_exec(|| {
                    // Under `nohup` SIGHUP is inherited ignored, which would keep the control
                    // alive; the binding program installs a handler, the control gets the
                    // default.
                    nix::sys::signal::signal(Signal::SIGHUP, nix::sys::signal::SigHandler::SigDfl)
                        .map_err(std::io::Error::from)?;
                    nix::sys::prctl::set_pdeathsig(Signal::SIGHUP).map_err(std::io::Error::from)
                })
            };
            (binding, control.spawn().expect("the control starts"))
        })
        .await
        .expect("the pool thread's work")
    });
    // The pool thread idles out; the control, whose parent it was, is signaled and dies.
    let status = eventually(
        || control.try_wait().expect("wait for the control"),
        || "the control process to exit".to_string(),
    );
    assert_eq!(status.signal(), Some(Signal::SIGHUP as i32), "{status}");
    assert!(alive(binding.pid()), "the binding outlives the pool thread");
    let stopped = runtime.block_on(binding.stop(STOP_GRACE));
    assert!(stopped.group_empty && !stopped.killed, "{stopped:?}");
}

// ---------------------------------------------------------------------------- stopping

/// A binding serving a kernel, stopped as a session's shutdown stops it.
#[test]
fn a_well_behaved_binding_stops_within_the_grace_without_sigkill() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    assert_eq!(relay.output(1, &with_root("print(root.method(1))")), "2\n");
    let pgid = Pid::from_raw(relay.binding_pid("fx") as i32);
    let started = Instant::now();
    let stopped = relay.stop_binding("fx", STOP_GRACE);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    assert!(stopped.group_empty && !stopped.killed, "{stopped:?}");
    assert_eq!(
        stopped.status.and_then(|status| status.signal()),
        Some(Signal::SIGTERM as i32)
    );
    assert!(group_gone(pgid));
}

#[test]
fn a_binding_that_ignores_sigterm_is_killed_with_its_group_after_the_grace() {
    let runtime = runtime();
    let binding = runtime
        .block_on(supervisor::start(spec(
            "outrig_fixture:make_stubborn",
            None,
        )))
        .expect("the binding starts");
    let pgid = binding.pid();
    let members = eventually(
        || {
            let members = group_members(pgid);
            members
                .iter()
                .any(|member| member.comm == "sleep")
                .then_some(members)
        },
        || "the stubborn child".to_string(),
    );
    assert!(members.len() >= 3, "python3, sh and sleep: {members:?}");
    let grace = Duration::from_millis(500);
    let started = Instant::now();
    let stopped = runtime.block_on(binding.stop(grace));
    assert!(started.elapsed() >= grace, "{:?}", started.elapsed());
    assert!(stopped.killed && stopped.group_empty, "{stopped:?}");
    assert!(stopped.survivors.is_empty(), "{:?}", stopped.survivors);
    assert_eq!(
        stopped.status.and_then(|status| status.signal()),
        Some(Signal::SIGKILL as i32)
    );
    assert!(group_gone(pgid));
}

#[test]
fn closing_the_bindings_stdin_ends_its_group() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let pgid = Pid::from_raw(relay.binding_pid("fx") as i32);
    let started = Flag::new();
    relay.submit_in(
        "primary",
        1,
        &with_root(&format!("root.sleep_child(1000, started={})", started.py())),
    );
    started.wait();
    assert!(
        group_members(pgid)
            .iter()
            .any(|member| member.comm == "sleep")
    );
    relay.close_binding_stdin("fx");
    // The binding ends its group, itself included; its own exit is the owner's to reap.
    let deadline = Instant::now() + GONE_WITHIN;
    loop {
        let left = group_members(pgid);
        if left.len() == 1 && left[0].pid == pgid && left[0].state == 'Z' {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "group {pgid} still holds {left:?} after {GONE_WITHIN:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stopped = relay.stop_binding("fx", Duration::from_millis(200));
    assert!(stopped.group_empty && !stopped.killed, "{stopped:?}");
    assert_eq!(
        stopped.status.and_then(|status| status.signal()),
        Some(Signal::SIGKILL as i32),
        "killed by its own hand"
    );
}

#[test]
fn a_factory_that_fails_reports_the_bindings_stderr_and_leaves_nothing() {
    let runtime = runtime();
    let dir = tempfile::tempdir().expect("a directory");
    let pgid_file = dir.path().join("pgid");
    let env: Vec<(OsString, OsString)> = std::env::vars_os()
        .chain([(
            OsString::from("OUTRIG_FIXTURE_PGID_FILE"),
            pgid_file.clone().into_os_string(),
        )])
        .collect();
    let error = runtime
        .block_on(supervisor::start(spec(
            "outrig_fixture:make_failing",
            Some(&env),
        )))
        .expect_err("the start fails")
        .to_string();
    assert!(
        error.contains("outrig_fixture:make_failing")
            && error.contains("RuntimeError: the factory failed on purpose"),
        "{error}"
    );
    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("the factory wrote its group")
        .trim()
        .parse()
        .expect("a pid");
    wait_group_gone(Pid::from_raw(pgid), GONE_WITHIN);
}

/// A `start` dropped while the factory runs -- under a timeout, say -- ends the group the
/// factory had started programs in, and not the binding alone.
#[test]
fn a_start_dropped_while_the_factory_runs_leaves_nothing() {
    let runtime = runtime();
    let dir = tempfile::tempdir().expect("a directory");
    let pgid_file = dir.path().join("pgid");
    let env: Vec<(OsString, OsString)> = std::env::vars_os()
        .chain([(
            OsString::from("OUTRIG_FIXTURE_PGID_FILE"),
            pgid_file.clone().into_os_string(),
        )])
        .collect();
    let spec = spec("outrig_fixture:make_slow", Some(&env));
    let pgid = runtime.block_on(async {
        let mut start = Box::pin(supervisor::start(spec));
        let written = async {
            loop {
                match std::fs::read_to_string(&pgid_file) {
                    Ok(text) if !text.is_empty() => break text,
                    _ => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        };
        tokio::select! {
            result = &mut start => panic!("the start finished: {result:?}"),
            text = written => {
                drop(start);
                Pid::from_raw(text.trim().parse().expect("a pid"))
            }
        }
    });
    // The runtime keeps turning here, which is what reaps the leader tokio was handed.
    runtime.block_on(async {
        let deadline = Instant::now() + GONE_WITHIN;
        while !group_gone(pgid) {
            assert!(
                Instant::now() < deadline,
                "group {pgid} still holds {:?} after {GONE_WITHIN:?}",
                group_members(pgid)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

// ---------------------------------------------------------------------------- the environment

#[test]
fn the_environment_is_inherited_unless_one_is_given() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let home = std::env::var("HOME").expect("HOME");
    let out = relay.output(
        1,
        &with_root("print(root.env('HOME'), root.env('OUTRIG_TEST_ONLY'))"),
    );
    assert_eq!(out, format!("{home} None\n"));

    let path = std::env::var("PATH").expect("PATH");
    relay.bind_opts(
        "exact",
        &Bind {
            factory: FACTORY,
            exact_env: Some(&[
                ("PATH", path.as_str()),
                ("PYTHONPATH", "/nonexistent"),
                ("OUTRIG_TEST_ONLY", "given"),
            ]),
            ..Bind::default()
        },
    );
    // `-I` keeps PYTHONPATH from the binding's own interpreter; the programs it starts see it.
    let out = relay.output(
        2,
        &py(r#"
        import __main__
        root = __main__._kernels['primary'].hosted('exact')
        print(root.env('HOME'), root.env('OUTRIG_TEST_ONLY'), root.child_env('PYTHONPATH'),
              '/nonexistent' in root.sys_path())
        "#),
    );
    assert_eq!(out, "None given /nonexistent False\n");
}

// ---------------------------------------------------------------------------- the stdio

#[test]
fn frames_an_event_and_a_held_decision_interleave_on_one_binding() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    relay.open("child");
    relay.submit_in(
        "primary",
        1,
        &with_root("print('decided', root.decide({'ask': 'push?'}))"),
    );
    let decision = relay.decision("fx", TIMEOUT);
    assert_eq!(decision.request, json!({"ask": "push?"}));
    // Another agent's frames keep crossing while the request is held.
    assert_eq!(
        relay.output_in("child", 2, &with_root_in("child", "print(root.method(1))")),
        "2\n"
    );
    relay.output_in(
        "child",
        3,
        &with_root_in("child", "root.event({'kind': 'noted', 'n': 3})"),
    );
    assert_eq!(
        relay.event("fx", TIMEOUT),
        json!({"t": "event", "kind": "noted", "n": 3})
    );
    assert!(
        relay
            .try_result_from("primary", 1, Duration::from_millis(300))
            .is_none(),
        "the request is held until the answer"
    );
    relay.answer("fx", decision.id, json!({"allow": true}));
    let result = relay.result_from("primary", 1, TIMEOUT);
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(result["output"], "decided {'allow': True}\n");
}

/// A payload cannot take a decision's id or a line's kind, and one the owner would drop unread
/// -- past the line bound -- raises in the caller instead of leaving it waiting for ever.
#[test]
fn a_decision_payload_cannot_forge_the_envelope_or_pass_the_line_bound() {
    let mut relay = Relay::start();
    relay.bind("fx", FACTORY);
    let out = relay.output(
        1,
        &with_root(
            r#"
            for payload in ({'id': 999}, {'t': 'rpc'}, {'big': 'x' * (2 << 20)}):
                try:
                    root.decide(payload)
                except ValueError as e:
                    print(e.args[0])
            try:
                root.event({'t': 'rpc'})
            except ValueError as e:
                print(e.args[0])
            try:
                root.decide_path(b'/tmp/x\xff')
            except ValueError as e:
                print(e.args[0])
            "#,
        ),
    );
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 5, "{out}");
    assert!(
        lines[4].starts_with("the decision payload holds text that is not UTF-8"),
        "{out}"
    );
    assert_eq!(lines[0], "the decision payload may not carry ['id']");
    assert_eq!(lines[1], "the decision payload may not carry ['t']");
    assert!(
        lines[2].starts_with("the decision line is ")
            && lines[2].ends_with(" bytes, past the 1048576-byte bound"),
        "{out}"
    );
    assert_eq!(lines[3], "the event payload may not carry ['t']");
    // Nothing waits on the owner for any of them, and the table carries no stale entry: the
    // next decision is numbered after them and is the one the owner sees.
    relay.submit_in(
        "primary",
        2,
        &with_root("print(root.decide({'ask': 'next?'}))"),
    );
    let decision = relay.decision("fx", TIMEOUT);
    assert_eq!(decision.request, json!({"ask": "next?"}));
    relay.answer("fx", decision.id, json!("yes"));
    assert_eq!(relay.result_from("primary", 2, TIMEOUT)["output"], "yes\n");
}

// ---------------------------------------------------------------------------- through podman

#[cfg(feature = "e2e")]
mod e2e {
    use super::*;
    use crate::container::{Container, ContainerLaunchSpec, ContainerMount, ExecOptions};
    use crate::image::ImageTag;
    use crate::python::install::{Install, install};
    use crate::python::payload;
    use crate::python::testing::{ALPINE, pip_env, pull, wheel_links};

    /// The install directory, mounted read-only at its host path as a session mounts it,
    /// imports with the payload's interpreter in a container.
    #[tokio::test]
    async fn the_binding_cache_mounted_read_only_imports_in_a_container() {
        let root = tempfile::tempdir().expect("a root");
        let dir = install(
            root.path(),
            Install {
                python: python(),
                requires: &["pure==1.0".to_string()],
                env: &pip_env(wheel_links()),
            },
        )
        .await
        .expect("the install");
        let at = dir.to_str().expect("a UTF-8 path").to_string();
        pull(ALPINE).await;
        let mut launch = ContainerLaunchSpec::default();
        launch
            .mounts
            .push(payload::mount().await.expect("the payload"));
        launch
            .mounts
            .push(ContainerMount::shared_read_only(dir.clone(), &at));
        let mut container = Container::start(&ImageTag::new(ALPINE), launch)
            .await
            .expect("the container starts");
        container
            .bootstrap_user()
            .await
            .expect("the user bootstraps");
        let python = format!("{}/bin/python3", payload::PAYLOAD_MOUNT);
        let run = |program: String| {
            let argv = vec![python.clone(), "-I".to_string(), "-c".to_string(), program];
            let container = &container;
            async move {
                container
                    .exec_capture(&argv, &ExecOptions::new())
                    .await
                    .expect("python runs in the container")
            }
        };
        let output = run(format!(
            "import sys; sys.path.insert(0, {at:?}); import pure; print(pure.ANSWER, pure.__file__)"
        ))
        .await;
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("42 {at}/pure.py\n"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let output = run(format!("open({:?}, 'w')", format!("{at}/written"))).await;
        assert!(
            !output.status.success()
                && String::from_utf8_lossy(&output.stderr).contains("Read-only file system"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        container
            .stop(Duration::from_secs(2))
            .await
            .expect("the container stops");
    }

    /// The realistic case, with network: GitPython 3.2.0 installs from the index, pure Python
    /// throughout, and a `Repo` the binding process builds over a test repository answers the
    /// interpreter through the relay.
    #[test]
    fn gitpython_installs_and_the_interpreter_reads_head_commit_hexsha() {
        let root = tempfile::tempdir().expect("a root");
        let dir = runtime()
            .block_on(install(
                root.path(),
                Install {
                    python: python(),
                    requires: &["GitPython==3.2.0".to_string()],
                    env: &[],
                },
            ))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            dir.join("gitpython-3.2.0.dist-info").is_dir() && dir.join("git/__init__.py").is_file()
        );

        let repo = tempfile::tempdir().expect("a repository");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=outrig@example.com",
            "-c",
            "user.name=outrig",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        let head = git(&["rev-parse", "HEAD"]);
        let tree = repo.path().canonicalize().expect("the repository's path");

        let mut relay = Relay::start();
        relay.bind_opts(
            "repo",
            &Bind {
                factory: "git:Repo",
                packages: Some(&dir),
                cwd: Some(&tree),
                ..Bind::default()
            },
        );
        let out = relay.output(
            1,
            &py(r#"
            import __main__
            repo = __main__._kernels['primary'].hosted('repo')
            print(repo.head.commit.hexsha, repo.working_tree_dir)
            "#),
        );
        assert_eq!(out, format!("{head} {}\n", tree.display()));
    }
}
