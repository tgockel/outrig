//! A Python program on the host as an owner: `outrig run <script.py>`.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use nix::sys::signal::Signal;
use serde_json::{Map, Value, json};

use super::builder::chosen_repo_root;
use crate::config::{Config, LlmProvider};
use crate::error::OutrigError;
use crate::python::program::{self, Program, Spec};

/// A trusted Python program to run on the host, under the Python OutRig carries, as
/// `outrig run <script.py> [args...]` runs one.
///
/// The program is the owner's code, not a sandbox: it runs with this process's authority, its
/// environment, working directory and stdio, which it inherits, and its arguments, which are its
/// own. It runs as `__main__`, imports modules beside its script, and imports `outrig`, where
/// `outrig.config()` is a read-only view of the [`Config`] given here, holding no secret.
/// Starting it starts no container and calls no model.
///
/// It leads a process group of its own, and ends with this process: killed, this process takes
/// the program's group with it. It does not read the terminal, which its group does not hold;
/// a read from it fails.
#[derive(Debug)]
pub struct HostProgram {
    config: Config,
    script: PathBuf,
    args: Vec<OsString>,
    repo_root: Option<PathBuf>,
}

impl HostProgram {
    /// The program at `script`, given `config`, loaded and validated by the caller: what
    /// `outrig.config()` shows it.
    pub fn new(config: Config, script: impl Into<PathBuf>) -> Self {
        Self {
            config,
            script: script.into(),
            args: Vec::new(),
            repo_root: None,
        }
    }

    /// The program's arguments, after its script in its `sys.argv`.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// The repository the configuration belongs to, which a relative `[workspace]` host path
    /// resolves against; the working directory when not given.
    pub fn repo_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.repo_root = Some(root.into());
        self
    }

    /// Start the program. A script that cannot be opened as a file fails this before anything
    /// is unpacked or started, as does a configured workspace whose path is not UTF-8, which
    /// `outrig.config()` could not show.
    pub async fn start(self) -> Result<RunningProgram, OutrigError> {
        let repo_root = chosen_repo_root(self.repo_root.as_deref())?;
        let program = program::start(Spec {
            script: &self.script,
            args: &self.args,
            config: view(&self.config, &repo_root)?,
            cwd: None,
            #[cfg(test)]
            adjust: None,
        })
        .await?;
        Ok(RunningProgram { program })
    }
}

/// A host program while it runs. Dropped before [`RunningProgram::wait`] returns, it kills the
/// program's process group.
#[derive(Debug)]
pub struct RunningProgram {
    program: Program,
}

impl RunningProgram {
    /// Send `SIGINT` to the program's process group, as a terminal's Ctrl-C would reach it had
    /// it been in the foreground. An owner forwards its own `SIGINT` this way; nothing here
    /// installs a handler.
    pub fn interrupt(&self) {
        self.program.signal(Signal::SIGINT);
    }

    /// Send `SIGTERM` to the program's process group.
    pub fn terminate(&self) {
        self.program.signal(Signal::SIGTERM);
    }

    /// Wait for the program to exit. Cancel-safe. What it started and left running in its group
    /// is not waited for.
    pub async fn wait(&mut self) -> Result<ProgramExit, OutrigError> {
        Ok(ProgramExit {
            status: self.program.wait().await?,
        })
    }
}

/// How a host program ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProgramExit {
    status: ExitStatus,
}

impl ProgramExit {
    /// The program's own exit status.
    pub fn status(&self) -> ExitStatus {
        self.status
    }

    /// The status for an owner to exit with: the program's, or 128 plus the signal's number when
    /// a signal ended it, as a shell reports one.
    pub fn code(&self) -> i32 {
        match (self.status.code(), self.status.signal()) {
            (Some(code), _) => code,
            (None, Some(signal)) => 128 + signal,
            (None, None) => 1,
        }
    }
}

/// What `outrig.config()` answers with: the models, aliases, providers and defaults of `config`,
/// and nothing that is or names a secret -- no key, no `${VAR}`, and nothing of the images,
/// sidecars or bindings, whose values may be literal ones.
///
/// A declared workspace whose path is not UTF-8 is a configuration error: the view is JSON, which
/// holds text, and a path with a byte replaced would name another directory.
pub(crate) fn view(config: &Config, repo_root: &Path) -> Result<Value, OutrigError> {
    let mut models = Map::new();
    let mut aliases = Map::new();
    for (name, model) in &config.models {
        match &model.alias {
            Some(targets) => {
                // Flattened as a call walks them; a configuration nobody validated may have no
                // such order, and shows what it declares.
                let candidates = config.model_candidates(name).map_or_else(
                    |_| targets.clone(),
                    |candidates| candidates.into_iter().map(str::to_owned).collect(),
                );
                aliases.insert(name.clone(), json!({ "candidates": candidates }));
            }
            None => {
                models.insert(
                    name.clone(),
                    json!({
                        "provider": model.provider,
                        "identifier": model.identifier.as_deref().unwrap_or(name),
                        "context_window": model.context_window,
                        "max_tokens": model.max_tokens,
                    }),
                );
            }
        }
    }
    let providers: Map<String, Value> = config
        .providers
        .iter()
        .map(|(name, provider)| {
            let (LlmProvider::OpenAi { base_url, .. } | LlmProvider::Anthropic { base_url, .. }) =
                provider;
            let row = json!({ "style": provider.style(), "base_url": base_url });
            (name.clone(), row)
        })
        .collect();
    // As a session mounts it: resolved against its base, `..` kept for the filesystem to follow,
    // through a symlink as the mount does.
    let workspace = config
        .workspace
        .declared_host_path()
        .map(|_| {
            let path = config.workspace.resolved_host_path(repo_root);
            path.into_os_string().into_string().map_err(|path| {
                OutrigError::Configuration(format!(
                    "[workspace] host-path resolves to {:?}, which is not UTF-8 and so cannot be \
                     shown as outrig.config().workspace",
                    Path::new(&path)
                ))
            })
        })
        .transpose()?;
    Ok(json!({
        "models": models,
        "aliases": aliases,
        "providers": providers,
        "default_model": config.default_model,
        "workspace": workspace,
        "events": { "mode": config.events.mode() },
    }))
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::ExitStatus;

    use super::ProgramExit;

    /// The owner exits with the program's status, or 128 plus the number of the signal that
    /// ended it, as a shell reports one: `sys.exit(1)` and a death by a signal are never the same.
    #[test]
    fn the_code_is_the_programs_or_128_plus_its_signal() {
        let code = |raw| {
            ProgramExit {
                status: ExitStatus::from_raw(raw),
            }
            .code()
        };
        assert_eq!(code(0), 0);
        assert_eq!(code(2 << 8), 2);
        assert_eq!(code(1 << 8), 1);
        assert_eq!(code(2), 130);
        assert_eq!(code(15), 143);
        assert_eq!(code(9 | 0x80), 137, "a core dump is still the signal's");
    }
}
