//! A session's bindings: gathered from its configuration and its owner, approved where a
//! repository declared them, installed, started before the interpreter, described to it and to
//! the agent, and stopped at shutdown (`plan/phase/0003-python/hosted-objects.md`).
//!
//! Each binding runs in a process of its own on the host, which `python::supervisor` starts and
//! stops. Its order -- the global config's, then the repository's, then the owner's, by name
//! within each -- is the order the agent is told of them in, and the order their package
//! directories go on the interpreter's `sys.path`, which decides which version of a package two
//! bindings share the container imports.

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
use crate::python::supervisor::{self, STOP_GRACE, Spec};

/// What [`SessionBuilder::bind`](super::SessionBuilder::bind) takes: a binding, as a
/// `[bindings.<name>]` entry holds one, and optionally the environment its process runs with.
#[derive(Debug, Clone)]
pub struct BindingSpec {
    binding: Binding,
    env: Option<Vec<(OsString, OsString)>>,
}

impl BindingSpec {
    /// `binding`, its process inheriting the owner's environment less every variable the
    /// session's configuration names through `${VAR}`, which are OutRig's own secrets.
    pub fn new(binding: Binding) -> Self {
        Self { binding, env: None }
    }

    /// Run the binding's process with exactly `vars` as its environment. The only credentials it
    /// then holds are those in `vars`, which keeps them apart from every other binding's.
    pub fn env<K, V>(mut self, vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.env = Some(
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        );
        self
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
    /// What the session runs on, for the variables a binding's inherited environment loses.
    pub(crate) config: &'a Config,
    /// What every relative `{ path }` resolves against.
    pub(crate) repo_root: &'a Path,
    /// Where each binding process starts: the workspace's host directory.
    pub(crate) cwd: &'a Path,
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

/// Where binding processes run from: the payload's interpreter and the vendored RPyC.
pub(crate) struct Programs {
    pub(crate) python: PathBuf,
    pub(crate) rpyc_dir: PathBuf,
}

impl Programs {
    pub(crate) async fn find() -> Result<Self> {
        Ok(Self {
            python: payload::host_python().await?,
            rpyc_dir: payload::rpyc_dir().await?,
        })
    }
}

/// How the session starts `declared`'s process: its factory's arguments with every `{ path }`
/// made an absolute host path, its packages, `serialize`, and its environment -- the owner's
/// given one exactly, or else `inherited`.
pub(crate) fn lower<'a>(
    declared: &'a Declared,
    packages: &'a Path,
    programs: &'a Programs,
    cwd: &'a Path,
    repo_root: &Path,
    inherited: Option<&'a [(OsString, OsString)]>,
) -> Result<Spec<'a>> {
    let binding = &declared.spec.binding;
    let (args, kwargs) = binding.arguments(repo_root).map_err(|e| {
        OutrigError::Configuration(format!("the binding `{}`'s arguments: {e}", declared.name))
    })?;
    Ok(Spec {
        name: &declared.name,
        python: &programs.python,
        rpyc_dir: &programs.rpyc_dir,
        packages,
        factory: &binding.factory,
        args,
        kwargs,
        serialize: binding.serialize,
        cwd: Some(cwd),
        env: declared.spec.env.as_deref().or(inherited),
        #[cfg(test)]
        probe: None,
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

/// Start each of `declared`, whose packages are in `packages`, all at once, and wait for every
/// factory to return. A factory that raises fails the start, naming the binding and what it
/// raised, and the bindings that did start are stopped.
pub(crate) async fn start(
    declared: Vec<Declared>,
    packages: Vec<PathBuf>,
    setup: &Setup<'_>,
) -> Result<Bindings> {
    if declared.is_empty() {
        return Ok(Bindings::default());
    }
    let programs = Programs::find().await?;
    let inherited = inherited_env(setup.config);
    let started = futures_util::future::join_all(declared.iter().zip(&packages).map(
        |(binding, packages)| async {
            let spec = lower(
                binding,
                packages,
                &programs,
                setup.cwd,
                setup.repo_root,
                inherited.as_deref(),
            )?;
            supervisor::start(spec).await
        },
    ))
    .await;
    let mut bindings = Bindings::default();
    let mut failed = None;
    for ((declared, packages), started) in declared.into_iter().zip(packages).zip(started) {
        match started {
            Ok(process) => bindings.running.push(Running {
                packages: (!declared.spec.binding.requires.is_empty()).then_some(packages),
                declared,
                process,
            }),
            Err(e) => {
                failed.get_or_insert(e);
            }
        }
    }
    match failed {
        Some(e) => {
            bindings.stop_after_failed_start().await;
            Err(e)
        }
        None => Ok(bindings),
    }
}

/// A session's running bindings, in the session's order.
#[derive(Debug, Default)]
pub(crate) struct Bindings {
    running: Vec<Running>,
}

#[derive(Debug)]
struct Running {
    declared: Declared,
    /// Its package directory on the host, or `None` when it requires nothing.
    packages: Option<PathBuf>,
    process: supervisor::Binding,
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
        self.running.is_empty()
    }

    /// What the agent is told of each binding, in order.
    pub(crate) fn briefs(&self) -> Vec<Brief> {
        self.running
            .iter()
            .map(|running| Brief {
                name: running.declared.name.clone(),
                description: running.declared.spec.binding.description.clone(),
                type_name: running.process.type_name().to_string(),
                serialize: running.declared.spec.binding.serialize,
            })
            .collect()
    }

    /// Each distinct package directory on the host, in the bindings' order: the order they go
    /// on the interpreter's `sys.path`.
    pub(crate) fn package_dirs(&self) -> Vec<&Path> {
        let mut dirs: Vec<&Path> = Vec::new();
        for dir in self.running.iter().filter_map(|r| r.packages.as_deref()) {
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

    /// Stop every binding at once -- `SIGTERM` to its group, `grace`, `SIGKILL` to the group --
    /// and say how each stopped.
    pub(crate) async fn stop(&mut self, grace: Duration) -> Vec<BindingStop> {
        let running = std::mem::take(&mut self.running);
        futures_util::future::join_all(running.into_iter().map(|running| async move {
            let stopped = running.process.stop(grace).await;
            BindingStop {
                name: running.declared.name,
                group_empty: stopped.group_empty,
                killed: stopped.killed,
                survivors: stopped.survivors.iter().map(ToString::to_string).collect(),
            }
        }))
        .await
    }

    /// Stop every binding of a session that failed to start, warning of any group that could not
    /// be proven empty, since no report will.
    pub(crate) async fn stop_after_failed_start(&mut self) {
        for stopped in self.stop(STOP_GRACE).await {
            if !stopped.group_empty {
                tracing::warn!(
                    target: "outrig::harness",
                    "stopping a session that failed to start: the binding `{}`'s process group \
                     is not proven empty: {}",
                    stopped.name,
                    stopped.survivors.join(", ")
                );
            }
        }
    }

    /// The pid of the binding `name`, which is its process group's id, for a test.
    #[cfg(test)]
    pub(crate) fn pid(&self, name: &str) -> Option<nix::unistd::Pid> {
        self.running
            .iter()
            .find(|r| r.declared.name == name)
            .map(|r| r.process.pid())
    }
}

/// Where a package directory on the host appears in the container.
pub(crate) fn container_path(dir: &Path) -> PathBuf {
    Path::new(BINDINGS_MOUNT).join(dir.file_name().expect("an install directory has a name"))
}
