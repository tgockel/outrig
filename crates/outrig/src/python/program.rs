//! A Python program on the host, as `outrig run <script.py>` runs it: the payload's interpreter
//! running the operator's script as `__main__`, a supervised child of the engine with a private
//! connection to it.
//!
//! The program is the operator's, not a sandbox, and everything about it that a program run as
//! `python3 <script>` would own is its own: its argv, its working directory and environment, which
//! it inherits, its stdin, stdout and stderr, which it inherits and OutRig never reads, and its
//! exit status. `program.py` is the bootstrap that gets it there, given as the interpreter's `-c`
//! argument as `binding.py` is.
//!
//! - **Supervised.** Started through [`supervise`] from the one parent thread, so it leads a
//!   process group of its own and ends with the engine as a bindings process does. Being in a
//!   group of its own also makes it a background group on a terminal: Ctrl-C reaches the engine,
//!   which forwards it ([`Program::signal`]), and a read from the terminal would stop it. A host
//!   program does not read the terminal, so it starts with SIGTTIN and SIGTTOU ignored, and such
//!   a read fails at once with `EIO` rather than stopping it.
//! - **The connection.** Two pipes, one each way, whose program ends are its only inherited
//!   descriptors beyond stdio and are named on its command line. Each line is one JSON object;
//!   the program asks, and [`serve`] answers. The program marks both close-on-exec once it has
//!   them, so what it starts holds neither, and the engine learns of its exit from the process
//!   ([`Program::wait`]), never from a pipe a grandchild still holds.
//! - **The engine's end stays open** for as long as the program runs: the program reads its
//!   end's EOF as the engine's death and ends its group. So [`Program`] holds the write end
//!   itself, whatever becomes of the task serving it.

use std::ffi::OsString;
use std::io::PipeWriter;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::ExitStatus;

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::pipe;
use tokio::process::Command;
use tokio::task::JoinHandle;

use super::host::next_line;
use super::payload;
use super::supervisor::{Supervised, Supervision, quoted, spawn_supervised, supervise};
use crate::error::{OutrigError, Result};

/// The bootstrap as the payload's `-c` argument, staged by `build.rs` as `binding.py` is.
pub(crate) const PROGRAM: &str = include_str!(concat!(env!("OUT_DIR"), "/program.bootstrap"));
/// The longest request line the engine reads from the program; a longer one is discarded with a
/// warning. Requests are a kind and an id.
const LINE_MAX: usize = 64 << 10;

/// How to start a host program.
pub(crate) struct Spec<'a> {
    /// The script, as the operator gave it: what the program sees as `sys.argv[0]` and
    /// `__file__`.
    pub(crate) script: &'a Path,
    /// Everything after the script, which is the program's own.
    pub(crate) args: &'a [OsString],
    /// What `outrig.config()` answers with.
    pub(crate) config: Value,
    /// Its working directory, which a relative script is found from; this process's when
    /// `None`.
    pub(crate) cwd: Option<&'a Path>,
    /// A test's adjustments to the command before it is spawned: its environment, its stdio.
    #[cfg(test)]
    pub(crate) adjust: Option<Adjust>,
}

/// A test's adjustment to the program's command; see [`Spec::adjust`].
#[cfg(test)]
pub(crate) type Adjust = Box<dyn FnOnce(&mut Command) + Send>;

/// A running host program.
#[derive(Debug)]
pub(crate) struct Program {
    child: Supervised,
    /// The engine's write end, held open while the program runs; see the module doc.
    _held: PipeWriter,
    serving: JoinHandle<()>,
}

/// The bootstrap's command line, `python3 -I -c PROGRAM <package> <fd-in> <fd-out> <script>
/// [args...]`, with the two descriptors `inherited` -- close-on-exec here, so no other child of
/// this process inherits them -- kept open across the exec.
pub(crate) fn bootstrap(
    python: &Path,
    package: &Path,
    inherited: [RawFd; 2],
    script: &Path,
    args: &[OsString],
) -> std::process::Command {
    let mut cmd = std::process::Command::new(python);
    cmd.args(["-I", "-c", PROGRAM])
        .arg(package)
        .args(inherited.map(|fd| fd.to_string()))
        .arg(script)
        .args(args);
    // SAFETY: one async-signal-safe call per descriptor between fork and exec, allocating
    // nothing and taking no lock.
    unsafe {
        cmd.pre_exec(move || {
            for fd in inherited {
                if nix::libc::fcntl(fd, nix::libc::F_SETFD, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        })
    };
    cmd
}

/// Start the program `spec` describes. A script that cannot be read as a file is refused before
/// the payload is unpacked or anything starts.
pub(crate) async fn start(spec: Spec<'_>) -> Result<Program> {
    readable(
        &spec
            .cwd
            .map_or_else(|| spec.script.into(), |cwd| cwd.join(spec.script)),
    )?;
    let python = payload::host_python().await?;
    let package = payload::package_dir().await?;

    let (program_reads, engine_writes) = std::io::pipe()?;
    let (engine_reads, program_writes) = std::io::pipe()?;
    let inherited = [program_reads.as_raw_fd(), program_writes.as_raw_fd()];
    let mut cmd = Command::from(bootstrap(
        &python,
        &package,
        inherited,
        spec.script,
        spec.args,
    ));
    if let Some(cwd) = spec.cwd {
        cmd.current_dir(cwd);
    }
    supervise(&mut cmd, Supervision::default());
    // SAFETY: the hook runs in the child between fork and exec and makes only async-signal-safe
    // calls, allocating nothing and taking no lock.
    unsafe {
        cmd.pre_exec(|| {
            let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
            sigaction(Signal::SIGTTIN, &ignore)?;
            sigaction(Signal::SIGTTOU, &ignore)?;
            Ok(())
        })
    };
    #[cfg(test)]
    if let Some(adjust) = spec.adjust {
        adjust(&mut cmd);
    }

    let child = spawn_supervised(cmd).await.map_err(|e| {
        let why = format!("cannot start {}: {e}", python.display());
        cannot_run(spec.script, std::io::Error::new(e.kind(), why))
    })?;
    drop((program_reads, program_writes));

    let held = engine_writes.try_clone()?;
    let reads = pipe::Receiver::from_owned_fd(OwnedFd::from(engine_reads))?;
    let writes = pipe::Sender::from_owned_fd(OwnedFd::from(engine_writes))?;
    let serving = tokio::spawn(serve(reads, writes, spec.config));
    Ok(Program {
        child,
        _held: held,
        serving,
    })
}

impl Program {
    /// Send `signal` to its process group: what a terminal's key would reach had the program been
    /// in the foreground. Nothing once it has been reaped, when the group's id may name another.
    pub(crate) fn signal(&self, signal: Signal) {
        self.child.signal(signal);
    }

    /// Wait for the program to exit, and reap it. Cancel-safe. What it left running in its group
    /// is not waited for, and keeps running.
    pub(crate) async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        self.child.wait().await
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        // The child's own drop ends the group of a program never reaped: the engine is leaving
        // with it still running, which its own death detectors would see a moment later.
        self.serving.abort();
    }
}

/// Refuse a script that is not a file this process can open: nothing could run it. A directory
/// is refused too, though `python3` would run its `__main__.py`, because it would then go first
/// on `sys.path`.
fn readable(script: &Path) -> Result<()> {
    let refused = |e: std::io::Error| cannot_run(script, e);
    let file = std::fs::File::open(script).map_err(refused)?;
    if !file.metadata().map_err(refused)?.is_file() {
        return Err(refused(std::io::Error::other("it is not a file")));
    }
    Ok(())
}

/// Why the program at `script` did not start, as the launcher says it.
fn cannot_run(script: &Path, e: std::io::Error) -> OutrigError {
    let why = format!("cannot run the program {}: {e}", script.display());
    OutrigError::Io(std::io::Error::new(e.kind(), why))
}

/// Answer the program's requests, one line each, until it closes its end.
async fn serve(reads: pipe::Receiver, mut writes: pipe::Sender, config: Value) {
    let mut reads = BufReader::new(reads);
    let mut line = Vec::new();
    loop {
        match next_line(&mut reads, &mut line, LINE_MAX).await {
            Ok(Some(0)) => {}
            Ok(Some(cut)) => {
                tracing::warn!(
                    "ignored a line of more than {LINE_MAX} bytes from the host program ({cut} over)"
                );
                continue;
            }
            Ok(None) => return,
            Err(e) => {
                tracing::warn!("reading from the host program failed: {e}");
                return;
            }
        }
        let Some(reply) = answer(&line, &config) else {
            continue;
        };
        let mut reply = reply.to_string();
        reply.push('\n');
        if let Err(e) = writes.write_all(reply.as_bytes()).await {
            tracing::warn!("writing to the host program failed: {e}");
            return;
        }
    }
}

/// The reply to one request line, or `None` for a line that is not a request.
fn answer(line: &[u8], config: &Value) -> Option<Value> {
    let Ok(Value::Object(request)) = serde_json::from_slice::<Value>(line) else {
        tracing::warn!(
            "ignored a line from the host program that is not a request: {:?}",
            quoted(line)
        );
        return None;
    };
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    Some(match request.get("t").and_then(Value::as_str) {
        Some("config") => json!({"t": "config", "id": id, "config": config}),
        kind => json!({
            "t": "error",
            "id": id,
            "error": format!("the engine has no request {kind:?}"),
        }),
    })
}
