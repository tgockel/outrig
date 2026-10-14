//! A session's bindings: gathered from its configuration and its owner, approved where a
//! repository declared them, installed, started before the interpreter, described to it and to
//! the agent, and stopped at shutdown (`plan/phase/0003-python/hosted-objects.md`).
//!
//! One process on the host holds every binding of a session, which `python::supervisor` starts
//! and stops. Its order -- the global config's, then the repository's, then the owner's, by name
//! within each -- is the order the agent is told of them in, the order their factories are
//! called in, and the order their package directories go on `sys.path`, the process's and the
//! interpreter's alike, which decides which version of a package two bindings share both import.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::approval::{self, Approval, Approver, Store};
use super::event::Payload;
use super::report::BindingStop;
use crate::config::{Binding, Config};
use crate::error::{OutrigError, Result};
use crate::events::Events;
use crate::python::host::Interpreter;
use crate::python::install::{self, Install};
use crate::python::payload::{self, BINDINGS_MOUNT};
use crate::python::supervisor::{self, Factory, STOP_GRACE, Spec};

/// What [`SessionBuilder::bind`](super::SessionBuilder::bind) takes: a binding, as a
/// `[bindings.<name>]` entry holds one.
#[derive(Debug, Clone)]
pub struct BindingSpec {
    binding: Binding,
}

impl BindingSpec {
    /// `binding`, held by the session's bindings process with the environment that process has
    /// (see [`SessionBuilder::bindings_env`](super::SessionBuilder::bindings_env)).
    pub fn new(binding: Binding) -> Self {
        Self { binding }
    }
}

impl From<Binding> for BindingSpec {
    fn from(binding: Binding) -> Self {
        Self::new(binding)
    }
}

/// A binding the session is to start, in the session's order.
#[derive(Debug, Clone)]
pub(crate) struct Declared {
    pub(crate) name: String,
    pub(crate) spec: BindingSpec,
}

/// The bindings `config` declares and the owner gave, in the session's order: the global
/// config's and those built in code, then the repository's, then the owner's, by name within
/// each. Each is held to the config's rules, and a name the owner gives twice, or that the config
/// already declares, is refused rather than either shadowing the other. A repository entry that
/// replaced a global one of the same name is the repository's, and is no collision.
pub(crate) fn gather(config: &Config, given: &[(String, BindingSpec)]) -> Result<Vec<Declared>> {
    let (repo, operator): (Vec<_>, Vec<_>) = config
        .bindings
        .iter()
        .partition(|(_, binding)| binding.repo_declared());
    let mut declared: Vec<Declared> = operator
        .into_iter()
        .chain(repo)
        .map(|(name, binding)| Declared {
            name: name.clone(),
            spec: BindingSpec::new(binding.clone()),
        })
        .collect();
    let mut owner: Vec<&(String, BindingSpec)> = given.iter().collect();
    owner.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, spec) in owner {
        if let Some(taken) = declared.iter().find(|d| &d.name == name) {
            let by = match taken.spec.binding.declared_in() {
                Some(file) => format!("declared in {}", file.display()),
                None if config.bindings.contains_key(name) => {
                    "declared in the configuration".into()
                }
                None => "given through `bind` already".into(),
            };
            return Err(OutrigError::Configuration(format!(
                "the binding `{name}` given through `bind` is {by}: a binding's name is a global \
                 in every kernel, so it names one binding"
            )));
        }
        declared.push(Declared {
            name: name.clone(),
            spec: spec.clone(),
        });
    }
    for binding in &declared {
        crate::config::validate_binding(&binding.name, &binding.spec.binding)?;
    }
    Ok(declared)
}

/// Where a session's bindings keep what outlives it: the requirement sets' installs, the
/// variables pip's environment gains for them, and the store of approvals.
#[derive(Debug, Clone)]
pub(crate) struct Places {
    pub(crate) packages_root: PathBuf,
    pub(crate) pip_env: Vec<(OsString, OsString)>,
    /// `None` remembers nothing.
    pub(crate) store: Option<Store>,
}

impl Places {
    /// The user's: installs in the cache directory, pip as the user configured it, and
    /// approvals in the state directory.
    pub(crate) fn user() -> Result<Self> {
        Ok(Self {
            packages_root: install::bindings_dir()?,
            pip_env: Vec::new(),
            store: Store::user(),
        })
    }
}

/// What starting a session's bindings needs from outside.
pub(crate) struct Setup<'a> {
    /// What the session runs on, for the variables an inherited environment loses.
    pub(crate) config: &'a Config,
    /// What every relative `{ path }` resolves against.
    pub(crate) repo_root: &'a Path,
    /// Where the bindings process starts: the workspace's host directory.
    pub(crate) cwd: &'a Path,
    /// Exactly the bindings process's environment, as the owner gave it; `None` inherits the
    /// owner's, less the config's secrets.
    pub(crate) env: Option<&'a [(OsString, OsString)]>,
    pub(crate) approver: Option<Arc<dyn Approver>>,
    pub(crate) places: Places,
    pub(crate) events: &'a Events,
}

/// `declared`, less each binding a repository declared that is not approved. One whose exact
/// declaration was approved before in this repository is kept without asking; any other is put
/// to the approver, on a thread where it may block, and an approval is remembered. A binding that
/// is refused, or that nobody could be asked about, is dropped, and the session says which and
/// why in a `binding.refused` event.
pub(crate) async fn approve(declared: Vec<Declared>, setup: &Setup<'_>) -> Vec<Declared> {
    let store = setup.places.store.as_ref();
    let mut kept = Vec::with_capacity(declared.len());
    for binding in declared {
        if !binding.spec.binding.repo_declared() {
            kept.push(binding);
            continue;
        }
        let request = approval::request(&binding.name, &binding.spec.binding, setup.repo_root);
        if store.is_some_and(|store| store.approved(&request)) {
            kept.push(binding);
            continue;
        }
        let reason = match &setup.approver {
            None => "the session has no approver to ask about its declaration",
            Some(approver) => {
                let (approver, asked) = (Arc::clone(approver), request.clone());
                let answer = tokio::task::spawn_blocking(move || approver.approve(&asked))
                    .await
                    .unwrap_or(Approval::CannotAsk);
                match answer {
                    Approval::Approve => {
                        if let Some(Err(e)) = store.map(|store| store.record(&request)) {
                            tracing::warn!(
                                target: "outrig::harness",
                                "the approval of the binding `{}` could not be remembered, so \
                                 the next session asks again: {e}",
                                binding.name
                            );
                        }
                        kept.push(binding);
                        continue;
                    }
                    Approval::Refuse => "the operator did not approve its declaration",
                    _ => "nobody could be asked to approve its declaration",
                }
            }
        };
        let reason = format!(
            "{reason}; approve it in a session started at a terminal, or move \
             [bindings.{}] to the global config",
            binding.name
        );
        tracing::warn!(target: "outrig::harness", "the binding `{}` does not start: {reason}", binding.name);
        setup.events.emit(Payload::BindingRefused {
            binding: binding.name.clone(),
            declared_in: request.declared_in,
            reason,
        });
    }
    kept
}

/// Install each binding's requirement set, each distinct set once and all at once, and say where
/// each one's packages are, in `declared`'s order.
pub(crate) async fn install(declared: &[Declared], setup: &Setup<'_>) -> Result<Vec<PathBuf>> {
    let python = payload::host_python().await?;
    let root = &setup.places.packages_root;
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|e| OutrigError::Configuration(format!("create {}: {e}", root.display())))?;
    let installs = declared.iter().map(|binding| async {
        install::install(
            root,
            Install {
                python: &python,
                requires: &binding.spec.binding.requires,
                env: &setup.places.pip_env,
            },
        )
        .await
        .map_err(|e| {
            OutrigError::Configuration(format!(
                "the packages of the binding `{}` could not be installed: {e}",
                binding.name
            ))
        })
    });
    // Two bindings with one set wait on its lock, and the second finds the first's install.
    futures_util::future::try_join_all(installs).await
}

/// What the process holding `declared` is told of it: its factory, called with every `{ path }`
/// in its arguments made an absolute host path against `repo_root`.
pub(crate) fn lower(declared: &Declared, repo_root: &Path) -> Result<Factory> {
    let binding = &declared.spec.binding;
    let (args, kwargs) = binding.arguments(repo_root).map_err(|e| {
        OutrigError::Configuration(format!("the binding `{}`'s arguments: {e}", declared.name))
    })?;
    Ok(Factory {
        name: declared.name.clone(),
        factory: binding.factory.clone(),
        args,
        kwargs,
        serialize: binding.serialize,
    })
}

/// The owner's environment less every variable `config` names through `${VAR}`, or `None` --
/// inherit it whole -- when it names none.
fn inherited_env(config: &Config) -> Option<Vec<(OsString, OsString)>> {
    let secrets: BTreeSet<OsString> = config.env_refs().into_iter().map(OsString::from).collect();
    (!secrets.is_empty()).then(|| {
        std::env::vars_os()
            .filter(|(name, _)| !secrets.contains(name))
            .collect()
    })
}

/// Start the process holding `declared`, whose packages are in `packages`, and wait for every
/// factory to return, in order. A factory that raises fails the start, naming the binding and
/// what it raised, and nothing of the process is left running.
pub(crate) async fn start(
    declared: Vec<Declared>,
    packages: Vec<PathBuf>,
    setup: &Setup<'_>,
) -> Result<Bindings> {
    if declared.is_empty() {
        return Ok(Bindings::default());
    }
    let factories = declared
        .iter()
        .map(|declared| lower(declared, setup.repo_root))
        .collect::<Result<Vec<_>>>()?;
    let mut bindings = Bindings {
        packages: declared
            .iter()
            .zip(packages)
            .map(|(declared, dir)| (!declared.spec.binding.requires.is_empty()).then_some(dir))
            .collect(),
        declared,
        process: None,
    };
    let (python, rpyc_dir) = (payload::host_python().await?, payload::rpyc_dir().await?);
    let inherited = inherited_env(setup.config);
    let process = supervisor::start(Spec {
        python: &python,
        rpyc_dir: &rpyc_dir,
        paths: bindings.package_dirs(),
        bindings: factories,
        cwd: Some(setup.cwd),
        env: setup.env.or(inherited.as_deref()),
        #[cfg(test)]
        probe: None,
    })
    .await?;
    bindings.process = Some(process);
    Ok(bindings)
}

/// A session's bindings, in the session's order, and the process holding them.
#[derive(Debug, Default)]
pub(crate) struct Bindings {
    declared: Vec<Declared>,
    /// Each binding's package directory on the host, or `None` when it requires nothing.
    packages: Vec<Option<PathBuf>>,
    /// `None` once stopped, and for a session with no binding.
    process: Option<supervisor::Process>,
}

/// What the agent is told of one binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Brief {
    pub(crate) name: String,
    pub(crate) description: String,
    /// The qualified name of the type of the object its factory returned.
    pub(crate) type_name: String,
    pub(crate) serialize: bool,
}

impl Bindings {
    pub(crate) fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }

    /// What the agent is told of each binding, in order.
    pub(crate) fn briefs(&self) -> Vec<Brief> {
        self.declared
            .iter()
            .map(|declared| Brief {
                name: declared.name.clone(),
                description: declared.spec.binding.description.clone(),
                type_name: self
                    .process
                    .as_ref()
                    .map_or("", |process| process.type_name(&declared.name))
                    .to_string(),
                serialize: declared.spec.binding.serialize,
            })
            .collect()
    }

    /// Each distinct package directory on the host, in the bindings' order: the order they go
    /// on `sys.path`, the bindings process's and the interpreter's alike.
    pub(crate) fn package_dirs(&self) -> Vec<&Path> {
        let mut dirs: Vec<&Path> = Vec::new();
        for dir in self.packages.iter().filter_map(Option::as_deref) {
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
        dirs
    }

    /// Tell `interpreter` the bindings, with each package directory as `path` names it where the
    /// interpreter runs: in the container, or as itself for one on the host. Nothing, when there
    /// are none.
    pub(crate) fn declare(&self, interpreter: &Interpreter, path: impl Fn(&Path) -> PathBuf) {
        if self.is_empty() {
            return;
        }
        let paths = self
            .package_dirs()
            .into_iter()
            .map(|dir| path(dir).to_string_lossy().into_owned());
        interpreter.declare_bindings(&crate::python::host::bindings_message(
            &self.briefs(),
            paths,
        ));
    }

    /// Stop the bindings process -- `SIGTERM` to its group, `grace`, `SIGKILL` to the group --
    /// and say how it stopped; `None` when there was none, or it was already stopped.
    pub(crate) async fn stop(&mut self, grace: Duration) -> Option<BindingStop> {
        let process = self.process.take()?;
        let stopped = process.stop(grace).await;
        Some(BindingStop {
            names: self.declared.iter().map(|d| d.name.clone()).collect(),
            group_empty: stopped.group_empty,
            killed: stopped.killed,
            survivors: stopped.survivors.iter().map(ToString::to_string).collect(),
        })
    }

    /// Stop the bindings process of a session that failed to start, warning when its group could
    /// not be proven empty, since no report will.
    pub(crate) async fn stop_after_failed_start(&mut self) {
        if let Some(stopped) = self.stop(STOP_GRACE).await
            && !stopped.group_empty
        {
            tracing::warn!(
                target: "outrig::harness",
                "stopping a session that failed to start: the bindings process's group is not \
                 proven empty: {}",
                stopped.survivors.join(", ")
            );
        }
    }

    /// The bindings process's pid, which is its process group's id, for a test.
    #[cfg(test)]
    pub(crate) fn pid(&self) -> Option<nix::unistd::Pid> {
        self.process.as_ref().map(supervisor::Process::pid)
    }
}

/// Where a package directory on the host appears in the container.
pub(crate) fn container_path(dir: &Path) -> PathBuf {
    Path::new(BINDINGS_MOUNT).join(dir.file_name().expect("an install directory has a name"))
}
