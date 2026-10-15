//! The process holding a session's bindings, on the host: started, spoken to, and stopped with
//! everything it started.
//!
//! One process holds every binding of a session, each a library the operator chose, and a
//! library may start programs of its own -- GitPython runs `git` for nearly every call, and `git`
//! may run hooks. The property this module exists for is that the process stops, with everything
//! it started: at shutdown, and when the owner is killed with no chance to clean up. `Drop` does
//! not run on `SIGKILL`, so the tie is the operating system's (`lifecycle.md`), and four
//! mechanisms stack up:
//!
//! - **A process group of its own.** The process is started with `process_group(0)`, so it leads
//!   a group whose id is its pid. Every signal sent here goes to the group, which is what reaches
//!   the programs the libraries started and not only the process; a group of its own also keeps a
//!   terminal's Ctrl-C, which goes to the foreground group, from reaching it.
//! - **A parent-death signal.** `PR_SET_PDEATHSIG` delivers `SIGHUP` to the process when its
//!   parent dies, and its handler stops its group. prctl(2) means the *thread* that created the
//!   process, so every such process is forked from one process-global thread started on first
//!   use: a tokio blocking-pool thread exits after an idle timeout, a current-thread runtime has
//!   no second thread at all, and either way a thread that quits early would signal a process
//!   whose owner is alive. The spawn runs under the async caller's runtime handle, so the child's
//!   pipes and exit are driven by the caller's runtime as usual: a [`Process`] lives on the
//!   runtime that started it, as an `Owned` does.
//! - **The signal starts blocked.** Between `prctl` and the handler Python installs, a death
//!   would end the process by the signal's default action with nothing to stop its group. The
//!   pre-exec hook blocks `SIGHUP` first; the mask survives `exec`, so the signal stays pending
//!   until `binding.py`'s `main` unblocks it with the handler in place.
//! - **A parent check.** A death between `fork` and `prctl` is never signaled: the child is
//!   already an orphan, reparented to init or a subreaper. The hook compares `getppid()` with the
//!   owner's pid captured before the spawn and exits with status 3 when they differ.
//!
//! The process also treats end of input on stdin like the signal: the owner's death closes the
//! pipe, so a process whose signal was somehow lost still stops its group. Nothing here does
//! anything for that beyond holding stdin open until the queue's last sender is dropped.
//!
//! [`Process::stop`] is the orderly path: `SIGTERM` to the group, a grace, `SIGKILL` to the
//! group, the process reaped, and a report of whether the group is proven empty.
//!
//! [`crate::process::Cmd::spawn_owned`] is not used: `Cmd` takes a `&'static str` program and
//! has no pre-exec hook, and the child is signaled as a group, which `process.rs` deliberately
//! never does for the children sharing the owner's group. This is the second exception to that
//! module's rule, beside `spawn_stdio`, and keeps its promise the same way: dropping a
//! [`Process`] sends `SIGKILL` to its group before the drop returns.
//!
//! # What `group_empty` proves
//!
//! Only `kill(-pgid, 0)` failing with `ESRCH` proves the group empty. Success proves nothing: a
//! zombie is still a member until its parent reaps it, so a program a library started and did
//! not wait for stays in the group until init reaps it after the process is gone -- milliseconds
//! on a host whose PID 1 reaps orphans, and never on one that does not, where the report says
//! not proven rather than guess. A descendant that called `setsid` left the group and is outside
//! everything here.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::io::{self, ErrorKind};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc as std_mpsc};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, sigprocmask};
use nix::unistd::{Pid, getpid, getppid};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::host::{STDERR_LINE_MAX, STDERR_TAIL_LINES, next_line};
use crate::error::{OutrigError, Result};

/// The binding program as the payload's `-c` argument, staged by `build.rs` as `host.rs`'s
/// `PROGRAM` is.
pub(crate) const PROGRAM: &str = include_str!(concat!(env!("OUT_DIR"), "/binding.bootstrap"));
/// The grace a session's shutdown gives the bindings' group between `SIGTERM` and `SIGKILL`: the
/// time `host.rs` gives the interpreter's exec client to exit. `lifecycle.md` counts it after
/// the drain deadline rather than against it, which is the caller's to do.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long the process has to report ready, every factory included.
const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest protocol line the process writes: one 512 KiB part of a frame, base64-encoded, in
/// its JSON line, stays under 1 MiB. A longer line is discarded with a warning, as `host.rs`
/// discards one from the interpreter.
const LINE_MAX: usize = 1 << 20;
/// How much of a line that is not a message a diagnostic quotes.
const QUOTED: usize = 200;
/// Lines queued from the process before its reader waits.
const QUEUE: usize = 64;
/// Lines queued for the process's stdin beyond the one being written: one, so what waits for a
/// slow process waits in the relay's queue (`python::hosted`), bounded in bytes, where a request
/// can still be refused.
const STDIN_QUEUE: usize = 1;
/// What the process prefixes its own diagnostics with on stderr.
const DIAGNOSTIC: &str = "outrig-binding:";
/// How often `stop` looks at the group during the grace, and after `SIGKILL`.
const GRACE_POLL: Duration = Duration::from_millis(20);
const KILL_POLL: Duration = Duration::from_millis(10);
/// How long after `SIGKILL` the group has to empty before `stop` gives up on proving it.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// The signal the owner's death delivers to the process, which it catches.
const DEATH_SIGNAL: Signal = Signal::SIGHUP;

/// How to start the process that holds a session's bindings.
pub(crate) struct Spec<'a> {
    /// The payload's `python3`.
    pub(crate) python: &'a Path,
    /// The vendored RPyC's directory.
    pub(crate) rpyc_dir: &'a Path,
    /// The bindings' package directories, as [`super::install::install`] made them, in the order
    /// they go on the process's `sys.path`: where two hold different versions of one package,
    /// the first one's is imported.
    pub(crate) paths: Vec<&'a Path>,
    /// The bindings, in the order their factories are called.
    pub(crate) bindings: Vec<Factory>,
    /// The process's working directory; the owner's when `None`.
    pub(crate) cwd: Option<&'a Path>,
    /// Exactly the process's environment, or `None` to inherit the owner's -- the CLI's case, so
    /// the user's `ssh-agent`, credential helpers and `git` config reach the libraries. `-I` keeps
    /// `PYTHONPATH` and the other `PYTHON*` variables from changing the process's own interpreter
    /// either way; the programs it starts receive the environment as it is.
    pub(crate) env: Option<&'a [(OsString, OsString)]>,
    /// A test's view into the window between fork and exec; see [`Probe`].
    #[cfg(test)]
    pub(crate) probe: Option<Probe>,
}

/// One binding the process holds. Everything here crosses on the process's stdin, never its
/// command line, which any user on the host can read.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Factory {
    /// The binding's name, which the process serves it under.
    pub(crate) name: String,
    /// The factory, as `module:callable`.
    pub(crate) factory: String,
    /// The factory's positional arguments, a JSON array, and its keyword arguments, a JSON
    /// object.
    pub(crate) args: Value,
    pub(crate) kwargs: Value,
    /// One call at a time across the binding's connections.
    pub(crate) serialize: bool,
}

/// Test only: the child appends `forked <pid>` to the file at `path`, sleeps `delay` before the
/// parent-death signal is set -- so a test can kill the owner inside that window -- and then
/// appends `parent-ok` or `parent-changed`, whichever the check found.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct Probe {
    pub(crate) delay: Duration,
    pub(crate) path: std::path::PathBuf,
}

/// A request the process holds until [`Process::answer`] is called with its id.
#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the policy `0003-24` builds reads it")
)]
pub(crate) struct Decision {
    pub(crate) id: u64,
    /// The decision line less its `t` and `id`.
    pub(crate) request: Value,
}

/// How [`Process::stop`] ended.
#[derive(Debug)]
pub(crate) struct Stopped {
    /// The process's own exit status, when it could be reaped.
    #[cfg_attr(not(test), expect(dead_code, reason = "a test reads it"))]
    pub(crate) status: Option<ExitStatus>,
    /// Whether the group is proven empty: `kill(-pgid, 0)` found no process in it. Only that
    /// answer proves anything; a killed member is a zombie until its parent reaps it, so on a
    /// host whose PID 1 does not reap orphans the proof can fail with nothing running. A
    /// descendant that left the group with `setsid` is outside what it proves either way.
    pub(crate) group_empty: bool,
    /// Whether `SIGKILL` was needed.
    pub(crate) killed: bool,
    /// What was still in the group when the proof failed.
    pub(crate) survivors: Vec<Member>,
}

/// One process of a group, as `/proc/<pid>/stat` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) pid: Pid,
    /// The command name, up to 15 characters.
    pub(crate) comm: String,
    /// The state letter: `Z` for a zombie, `D` for an uninterruptible sleep.
    pub(crate) state: char,
}

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.pid, self.comm, self.state)
    }
}

/// The running process that holds a session's bindings: the queue to its stdin, and the lines it
/// writes, by kind.
#[derive(Debug)]
pub(crate) struct Process {
    child: Child,
    /// Its pid, which is also its process group's id.
    pid: Pid,
    /// `None` once [`Process::close_stdin`] was called.
    stdin: Option<mpsc::Sender<Value>>,
    rpc: Option<mpsc::Receiver<Value>>,
    events: Option<mpsc::Receiver<Value>>,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the policy `0003-24` builds reads it")
    )]
    decisions: Option<mpsc::Receiver<Decision>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// The qualified name of the type of what each binding's factory returned, by binding: the
    /// ready line's object.
    types: Value,
    /// Whether `stop` reaped the process, so `Drop` has nothing left to kill. Until then the
    /// unreaped process anchors the group's id, which names this group and no other.
    reaped: bool,
}

/// Start the process holding the bindings `spec` describes, and wait for every factory to
/// return.
///
/// The error for a factory that raises names its binding, the factory, and the exception's type
/// and message; for a process that exits, or says anything but `ready`, before then, it names the
/// bindings and carries the last of its stderr. Either way nothing of its group is left running.
pub(crate) async fn start(spec: Spec<'_>) -> Result<Process> {
    let mut cmd = Command::new(spec.python);
    cmd.args(["-I", "-c", PROGRAM]).arg(spec.rpyc_dir);
    if let Some(cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    if let Some(env) = spec.env {
        cmd.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);

    let owner = getpid();
    #[cfg(test)]
    let probe = spec.probe.as_ref().map(|probe| {
        let path = std::ffi::CString::new(probe.path.as_os_str().as_encoded_bytes())
            .expect("a probe path without NUL");
        let delay = nix::libc::timespec {
            tv_sec: probe.delay.as_secs() as nix::libc::time_t,
            tv_nsec: nix::libc::c_long::from(probe.delay.subsec_nanos()),
        };
        (path, delay)
    });
    // SAFETY: the hook runs in the child between fork and exec, a copy of a multi-threaded
    // process with every other thread gone mid-step, so everything it calls is
    // async-signal-safe -- open, write, close, nanosleep, sigprocmask, prctl, getppid and
    // _exit -- and it allocates nothing and takes no lock.
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(test)]
            if let Some((path, delay)) = &probe {
                note(path, b"forked ", Some(getpid()));
                nix::libc::nanosleep(delay, std::ptr::null_mut());
            }
            let mut blocked = SigSet::empty();
            blocked.add(DEATH_SIGNAL);
            sigprocmask(SigmaskHow::SIG_BLOCK, Some(&blocked), None)?;
            set_pdeathsig(DEATH_SIGNAL)?;
            if getppid() != owner {
                #[cfg(test)]
                if let Some((path, _)) = &probe {
                    note(path, b"parent-changed", None);
                }
                nix::libc::_exit(3);
            }
            #[cfg(test)]
            if let Some((path, _)) = &probe {
                note(path, b"parent-ok", None);
            }
            Ok(())
        })
    };

    // What did not start, when something stops the process: every binding, or the one whose
    // factory raised.
    let names: Vec<String> = spec
        .bindings
        .iter()
        .map(|binding| format!("`{}`", binding.name))
        .collect();
    let mut who = match names.as_slice() {
        [one] => format!("the binding {one}"),
        _ => format!("the bindings {}", names.join(", ")),
    };
    let armed = spawn_on_parent_thread(cmd).await.map_err(|e| {
        OutrigError::Configuration(format!(
            "{who} did not start: cannot run {}: {e}",
            spec.python.display()
        ))
    })?;
    let mut child = armed.into_child();
    let pid = Pid::from_raw(child.id().expect("a child not yet waited on") as i32);
    let stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let stderr = child.stderr.take().expect("stderr is piped");

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let drain = tokio::spawn(drain_stderr(stderr, Arc::clone(&tail)));
    let (stdin_tx, stdin_rx) = mpsc::channel(STDIN_QUEUE);
    let (rpc_tx, rpc_rx) = mpsc::channel(QUEUE);
    let (events_tx, events_rx) = mpsc::channel(QUEUE);
    let (decisions_tx, decisions_rx) = mpsc::channel(QUEUE);
    // The bindings go first, ahead of anything else the owner sends; the queue is empty, so this
    // cannot wait.
    let paths: Vec<_> = spec.paths.iter().map(|dir| dir.to_string_lossy()).collect();
    let _ = stdin_tx.try_send(json!({"t": "start", "paths": paths, "bindings": spec.bindings}));
    tokio::spawn(write_lines(stdin, stdin_rx));
    // The handle exists before anything waits, so a `start` dropped while a factory runs --
    // under a timeout, say -- takes the whole group with it rather than the process alone.
    let mut process = Process {
        child,
        pid,
        stdin: Some(stdin_tx),
        rpc: Some(rpc_rx),
        events: Some(events_rx),
        decisions: Some(decisions_rx),
        stderr: tail,
        types: Value::Null,
        reaped: false,
    };

    // The first line is the greeting, read here; the rest are the router's.
    let mut line = Vec::new();
    let first = tokio::time::timeout(READY_TIMEOUT, next_line(&mut stdout, &mut line, LINE_MAX));
    let why = match first.await {
        Err(_) => Some(format!("it did not report ready within {READY_TIMEOUT:?}")),
        Ok(Err(_)) | Ok(Ok(None)) => Some("its output closed before it reported ready".into()),
        Ok(Ok(Some(_))) => match serde_json::from_slice::<Value>(&line) {
            Ok(message) if message["t"] == "ready" => {
                process.types = message["types"].clone();
                None
            }
            Ok(message) if message["t"] == "failed" => {
                let name = message["binding"].as_str().unwrap_or_default();
                let factory = spec
                    .bindings
                    .iter()
                    .find(|binding| binding.name == name)
                    .map_or("", |binding| binding.factory.as_str());
                who = format!("the binding `{name}`");
                Some(format!(
                    "its factory `{factory}` raised {}",
                    message["error"]
                        .as_str()
                        .unwrap_or("an exception it could not describe")
                ))
            }
            Ok(message) => Some(format!("it said {} before ready", message["t"])),
            Err(_) => Some(format!(
                "its first line was not a protocol message: {:?}",
                quoted(&line)
            )),
        },
    };
    if let Some(why) = why {
        return Err(failed_start(process, drain, &who, why).await);
    }
    tokio::spawn(route(stdout, rpc_tx, events_tx, decisions_tx));
    Ok(process)
}

/// Kill the group of a process that never became ready, reap it, and say what happened, with
/// the last of its stderr; `what` names what did not start.
async fn failed_start(
    mut process: Process,
    drain: JoinHandle<()>,
    what: &str,
    why: String,
) -> OutrigError {
    let _ = kill(group(process.pid), Signal::SIGKILL);
    process.reaped = reap_by(&mut process.child, Instant::now() + KILL_WAIT)
        .await
        .is_some();
    let _ = tokio::time::timeout(Duration::from_secs(1), drain).await;
    let mut message = format!("{what} did not start: {why}");
    let stderr = process.stderr();
    if !stderr.is_empty() {
        let _ = write!(message, "; its stderr:\n{stderr}");
    }
    OutrigError::Configuration(message)
}

impl Process {
    /// The process's pid, which is also its process group's id.
    pub(crate) fn pid(&self) -> Pid {
        self.pid
    }

    /// The qualified name of the type of the object `binding`'s factory returned, such as
    /// `git.repo.base.Repo`; empty for a binding the process does not hold.
    pub(crate) fn type_name(&self, binding: &str) -> &str {
        self.types[binding].as_str().unwrap_or_default()
    }

    /// A sender into the stdin queue, for a std thread's `blocking_send`. Panics once
    /// [`Process::close_stdin`] was called.
    pub(crate) fn sender(&self) -> mpsc::Sender<Value> {
        self.stdin.clone().expect("the process's stdin is open")
    }

    /// Queue `message` as one line on the process's stdin -- an `rpc` frame, a decision's answer,
    /// a control line; `BrokenPipe` once stdin is closed or the process is gone.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the policy `0003-24` builds reads it")
    )]
    pub(crate) async fn send(&self, message: Value) -> io::Result<()> {
        let broken = || io::Error::new(ErrorKind::BrokenPipe, "the process's stdin is closed");
        let stdin = self.stdin.as_ref().ok_or_else(broken)?;
        stdin.send(message).await.map_err(|_| broken())
    }

    /// Answer decision `id` with `answer`, which releases the request the process held for it.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the policy `0003-24` builds reads it")
    )]
    pub(crate) async fn answer(&self, id: u64, answer: Value) -> io::Result<()> {
        self.send(json!({"t": "decision", "id": id, "answer": answer}))
            .await
    }

    /// The process's `rpc` lines, taken once.
    pub(crate) fn take_rpc(&mut self) -> mpsc::Receiver<Value> {
        self.rpc.take().expect("the rpc lines are taken once")
    }

    /// The process's `event` lines, taken once.
    pub(crate) fn take_events(&mut self) -> mpsc::Receiver<Value> {
        self.events.take().expect("the event lines are taken once")
    }

    /// The process's decision requests, taken once.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the policy `0003-24` builds reads it")
    )]
    pub(crate) fn take_decisions(&mut self) -> mpsc::Receiver<Decision> {
        self.decisions.take().expect("the decisions are taken once")
    }

    /// The last lines of the process's stderr.
    pub(crate) fn stderr(&self) -> String {
        tail_text(&self.stderr)
    }

    /// Drop the stdin queue, so stdin closes -- as the owner's death closes it -- once every
    /// [`Process::sender`] clone is dropped too.
    #[cfg(test)]
    pub(crate) fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Stop the process and everything it started: `SIGTERM` to its group, `grace` for the group
    /// to leave, `SIGKILL` to the group if it has not, the process reaped, and the report.
    pub(crate) async fn stop(mut self, grace: Duration) -> Stopped {
        let group = group(self.pid);
        let _ = kill(group, Signal::SIGTERM);
        let deadline = Instant::now() + grace;
        let mut status = None;
        loop {
            if status.is_none() {
                status = self.child.try_wait().ok().flatten();
            }
            // Only once the process itself is reaped can the group be empty.
            if status.is_some() && group_empty(group) {
                self.reaped = true;
                return Stopped {
                    status,
                    group_empty: true,
                    killed: false,
                    survivors: Vec::new(),
                };
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(GRACE_POLL).await;
        }
        // Before the reap: an unreaped process keeps the group's id from being reused.
        let _ = kill(group, Signal::SIGKILL);
        let deadline = Instant::now() + KILL_WAIT;
        if status.is_none() {
            // Bounded: a thread stuck in uninterruptible I/O -- a request to a hung FUSE
            // daemon, say -- outlasts `SIGKILL`, and the process is then left to `kill_on_drop`
            // and tokio's orphan queue, its group not proven empty.
            status = reap_by(&mut self.child, deadline).await;
        }
        self.reaped = status.is_some();
        while !group_empty(group) {
            if Instant::now() >= deadline {
                return Stopped {
                    status,
                    group_empty: false,
                    killed: true,
                    survivors: group_members(self.pid),
                };
            }
            tokio::time::sleep(KILL_POLL).await;
        }
        Stopped {
            status,
            group_empty: true,
            killed: true,
            survivors: Vec::new(),
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Not stopped, or stopped part way: the group dies now, while the unreaped process still
        // anchors its id. The process itself is then killed once more and reaped by
        // `kill_on_drop` and tokio's orphan queue; the group is what only this knows about.
        if !self.reaped {
            let _ = kill(group(self.pid), Signal::SIGKILL);
        }
    }
}

/// `child`'s exit status, if it can be reaped by `deadline`.
async fn reap_by(child: &mut Child, deadline: Instant) -> Option<ExitStatus> {
    tokio::time::timeout_at(deadline, child.wait())
        .await
        .ok()?
        .ok()
}

/// Every process whose group is `pgid`, by pid.
pub(crate) fn group_members(pgid: Pid) -> Vec<Member> {
    // The `/proc` listing may be unreadable, and an entry may vanish while it is read: either is
    // left out.
    let mut members: Vec<Member> = std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            // `pid (comm) state ppid pgrp ...`; comm may itself hold parentheses and spaces.
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            let open = stat.find('(')?;
            let close = stat.rfind(')')?;
            let mut fields = stat[close + 1..].split_whitespace();
            let state = fields.next()?.chars().next()?;
            let pgrp: i32 = fields.nth(1)?.parse().ok()?;
            (pgrp == pgid.as_raw()).then(|| Member {
                pid: Pid::from_raw(pid),
                comm: stat[open + 1..close].to_string(),
                state,
            })
        })
        .collect();
    members.sort_by_key(|member| member.pid);
    members
}

/// The id `kill` takes to signal `pid`'s whole group.
fn group(pid: Pid) -> Pid {
    Pid::from_raw(-pid.as_raw())
}

/// Whether nothing is left in `group`; see the module doc for what else `kill` could say.
fn group_empty(group: Pid) -> bool {
    kill(group, None) == Err(Errno::ESRCH)
}

/// A child whose group dies with this value until the child is taken out: what crosses from
/// the parent thread to the caller, so a `start` dropped in between -- its child made, its
/// reply never read -- still ends the group, where dropping the child alone would end the
/// leader and leave what its factory had started.
struct Armed(Option<Child>);

impl Armed {
    fn into_child(mut self) -> Child {
        self.0.take().expect("the child is present until taken")
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        if let Some(id) = self.0.as_ref().and_then(Child::id) {
            let _ = kill(group(Pid::from_raw(id as i32)), Signal::SIGKILL);
        }
    }
}

/// A spawn to run on the parent thread.
type Job = Box<dyn FnOnce() + Send>;

/// The one thread that parents every bindings process, started on first use. As with `supervise.rs`'s
/// reaper, only a successful start is remembered, so a thread that could not be started is
/// tried again by the next caller rather than failing every one.
fn parent_thread() -> io::Result<std_mpsc::Sender<Job>> {
    static PARENT: Mutex<Option<std_mpsc::Sender<Job>>> = Mutex::new(None);

    let mut started = PARENT.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(tx) = started.as_ref() {
        return Ok(tx.clone());
    }
    let (tx, rx) = std_mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("outrig-bind".to_string())
        .spawn(move || {
            for job in rx {
                // The thread's end would signal every process it started, so a panic in one
                // spawn is contained to that spawn, whose requester sees its reply dropped.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
        })?;
    *started = Some(tx.clone());
    Ok(tx)
}

/// Spawn `cmd` from the parent thread, under the calling runtime.
async fn spawn_on_parent_thread(mut cmd: Command) -> io::Result<Armed> {
    let handle = Handle::current();
    let (tx, rx) = oneshot::channel();
    let job: Job = Box::new(move || {
        let _runtime = handle.enter();
        // A reply nobody reads -- the requester's `start` dropped meanwhile -- drops the
        // `Armed` value, which ends the group.
        let _ = tx.send(cmd.spawn().map(|child| Armed(Some(child))));
    });
    parent_thread()?
        .send(job)
        .map_err(|_| io::Error::other("the thread that starts bindings is gone"))?;
    rx.await
        .map_err(|_| io::Error::other("the thread that starts bindings dropped the spawn"))?
}

/// Append `text`, then `pid` in decimal when given, then a newline to the probe file at `path`,
/// with nothing but `open`, `write` and `close`: this runs in the pre-exec hook.
#[cfg(test)]
fn note(path: &std::ffi::CStr, text: &[u8], pid: Option<Pid>) {
    use nix::libc;
    let mut buf = [0u8; 64];
    buf[..text.len()].copy_from_slice(text);
    let mut len = text.len();
    if let Some(pid) = pid {
        let mut digits = [0u8; 10];
        let mut rest = pid.as_raw().unsigned_abs();
        let mut count = 0;
        loop {
            digits[count] = b'0' + (rest % 10) as u8;
            count += 1;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        for digit in digits[..count].iter().rev() {
            buf[len] = *digit;
            len += 1;
        }
    }
    buf[len] = b'\n';
    len += 1;
    // SAFETY: `path` is NUL-terminated, `buf[..len]` is initialized, every call is
    // async-signal-safe, and the descriptor is this call's own.
    unsafe {
        let fd = libc::open(
            path.as_ptr(),
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        );
        if fd >= 0 {
            libc::write(fd, buf.as_ptr().cast(), len);
            libc::close(fd);
        }
    }
}

/// Write each queued message as one line to the process's stdin, which closes with the queue --
/// the process reads that as the owner's end.
async fn write_lines(mut stdin: ChildStdin, mut queue: mpsc::Receiver<Value>) {
    while let Some(message) = queue.recv().await {
        let mut line = message.to_string();
        line.push('\n');
        let written = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.flush().await
        };
        if let Err(e) = written.await {
            tracing::warn!("writing to the bindings process failed: {e}");
            return;
        }
    }
}

/// Hand each stdout line to the channel for its kind. A line nobody reads any more is dropped,
/// so the process is never held on a pipe with no reader.
async fn route(
    mut stdout: BufReader<ChildStdout>,
    rpc: mpsc::Sender<Value>,
    events: mpsc::Sender<Value>,
    decisions: mpsc::Sender<Decision>,
) {
    let mut line = Vec::new();
    loop {
        match next_line(&mut stdout, &mut line, LINE_MAX).await {
            Ok(Some(0)) => {}
            Ok(Some(cut)) => {
                tracing::warn!(
                    "ignored a line of more than {LINE_MAX} bytes from the bindings process ({cut} over)"
                );
                continue;
            }
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("reading from the bindings process failed: {e}");
                return;
            }
        }
        let Ok(Value::Object(mut message)) = serde_json::from_slice::<Value>(&line) else {
            tracing::warn!(
                "ignored a line from the bindings process that is not a protocol message: {:?}",
                quoted(&line)
            );
            continue;
        };
        match message.get("t").and_then(Value::as_str) {
            Some("rpc") => drop(rpc.send(Value::Object(message)).await),
            Some("event") => drop(events.send(Value::Object(message)).await),
            Some("decision") => {
                message.remove("t");
                let Some(id) = message.remove("id").and_then(|id| id.as_u64()) else {
                    tracing::warn!(
                        "ignored a decision line from the bindings process without an id"
                    );
                    continue;
                };
                let request = Value::Object(message);
                drop(decisions.send(Decision { id, request }).await);
            }
            kind => tracing::warn!("ignored a line of kind {kind:?} from the bindings process"),
        }
    }
}

/// Log the process's stderr line by line, its own diagnostics as warnings and the rest -- the
/// library's output -- at debug, keeping the last few to explain an exit.
async fn drain_stderr(stderr: ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    let mut stderr = BufReader::new(stderr);
    let mut line = Vec::new();
    while let Ok(Some(cut)) = next_line(&mut stderr, &mut line, STDERR_LINE_MAX).await {
        let mut text = String::from_utf8_lossy(&line).into_owned();
        if cut > 0 {
            let _ = write!(text, " [{cut} more bytes cut]");
        }
        if text.starts_with(DIAGNOSTIC) {
            tracing::warn!("{text}");
        } else {
            tracing::debug!("binding: {text}");
        }
        let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
        if tail.len() == STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(text);
    }
}

fn tail_text(tail: &Mutex<VecDeque<String>>) -> String {
    let mut tail = tail.lock().unwrap_or_else(PoisonError::into_inner);
    tail.make_contiguous().join("\n")
}

/// The start of `line`, as a diagnostic quotes it.
fn quoted(line: &[u8]) -> String {
    String::from_utf8_lossy(&line[..line.len().min(QUOTED)]).into_owned()
}
