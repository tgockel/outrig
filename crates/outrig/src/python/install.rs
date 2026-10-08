//! A binding's packages: its `requires`, installed once, pure Python only, into a cache keyed by
//! the requirement set.
//!
//! The payload's own pip installs the set into `bindings/<hash>/` under the cache root, which the
//! binding process imports from and the container later mounts read-only
//! (`hosted-objects.md`). Only pure Python can load on either side -- the payload is a static
//! build with no way to load an extension module -- and this pip offers only the bare
//! `linux_<arch>` platform tag (#465), so a package whose wheels are all compiled would fall back
//! to its source distribution. Two rules keep both out. Wheels only (`--only-binary=:all:`), so
//! no package's build code runs on the host. And every installed wheel checked to be pure Python,
//! since pip accepts a compiled wheel tagged for this platform: its platform tag must be `any`,
//! its ABI tag `none`, and its Python tag must include Python 3, as `py3-none-any` and
//! `py2.py3-none-any` have; and no extension module may be among its files, since a tag is the
//! publisher's description. A pure-Python package published only as a source distribution cannot
//! be hosted until a wheel of it exists.
//!
//! A requirement names a distribution on an index -- a name, extras, a version specifier,
//! markers -- and never a path or a URL: the cache is keyed by the requirement text, so a path
//! whose file changes would keep serving its first install. The install reads the user's pip
//! configuration (`PIP_*` variables, `pip.conf`) for the index, proxy and certificate settings
//! a network may require; the key stays the requirement set, so changing the configured index
//! reinstalls nothing, and removing the cache directory does.
//!
//! Nothing in production installs a requirement set yet; `0003-20` does.
#![cfg_attr(not(test), allow(dead_code))]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use regex::Regex;

use super::payload;
use crate::error::{IoPathExt, OutrigError, Result};
use crate::process::tail_string;

/// How much of pip's output an error carries.
const PIP_TAIL: usize = 4 * 1024;

/// pip, run as a program of its own so that its source-distribution step is disabled for the
/// run. `--only-binary=:all:` constrains what pip resolves from an index and nothing else: a
/// dependency a wheel declares by URL on a source archive is fetched, and the archive's build
/// backend run to read its metadata, which is code of the archive's choosing running on the
/// host. With the step disabled, such a candidate fails with the reason before anything of it
/// runs. The seam is pip's own and may move with a pip version; the payload pins pip, and a
/// test holds the seam to its job.
const PIP: &str = r#"
import sys

from pip._internal.cli.main import main
from pip._internal.distributions import sdist
from pip._internal.exceptions import InstallationError


def refuse(self, *args, **kwargs):
    raise InstallationError(
        f"{self.req} is a source distribution, and only pure-Python wheels are installed: "
        f"nothing is built on the host"
    )


sdist.SourceDistribution.prepare_distribution_metadata = refuse
sys.exit(main(sys.argv[1:]))
"#;

/// Why a wheel must be pure Python, as every refusal says it.
const RULE: &str = "only pure-Python wheels are installed (platform `any`, ABI `none`, Python 3), \
                    because the payload's interpreter loads no compiled code, on the host or in \
                    the container";

/// Where requirement sets are installed: `bindings/` under the cache root `payload.rs` chooses,
/// `~/.cache/outrig/bindings/` by default.
pub(crate) fn bindings_dir() -> Result<PathBuf> {
    Ok(payload::cache_dir("binding packages")?.join("outrig/bindings"))
}

/// One requirement set to install.
pub(crate) struct Install<'a> {
    /// The payload's `python3`, whose pip installs.
    pub(crate) python: &'a Path,
    /// Requirement specifiers, as a `[bindings.<name>]` entry's `requires` lists them.
    pub(crate) requires: &'a [String],
    /// Variables added to pip's environment, which is otherwise the owner's. Tests point pip at
    /// local wheels with `PIP_NO_INDEX` and `PIP_FIND_LINKS`; a session adds nothing.
    pub(crate) env: &'a [(OsString, OsString)],
}

/// The directory under `root` holding `install.requires`, installed once: concurrent callers
/// with the same set share one install, and a caller that waited on the lock uses the install
/// it finds. A failure names the requirement and the reason, and leaves no directory in `root`.
pub(crate) async fn install(root: &Path, install: Install<'_>) -> Result<PathBuf> {
    let set = requirement_set(install.requires)?;
    let dir = root.join(hash(&set));
    if dir.is_dir() {
        return Ok(dir);
    }
    let root = root.to_path_buf();
    let python = install.python.to_path_buf();
    let env = install.env.to_vec();
    let target = dir.clone();
    tokio::task::spawn_blocking(move || {
        payload::locked_once(&target, || {
            install_into(&root, &target, &python, &set, &env)
        })
    })
    .await
    .map_err(|e| OutrigError::Io(std::io::Error::other(e)))??;
    Ok(dir)
}

/// A requirement a binding may name: a distribution on an index, with optional extras, version
/// specifiers and markers, as PEP 508 writes them -- and not a path, a URL, or an option.
static SPECIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^
        [A-Za-z0-9][A-Za-z0-9._-]*                                 # the distribution's name
        (\s*\[[A-Za-z0-9._,\s-]*\])?                                # extras
        (\s*(===?|!=|~=|<=?|>=?)\s*[A-Za-z0-9._*+!-]+                # version specifiers
            (\s*,\s*(===?|!=|~=|<=?|>=?)\s*[A-Za-z0-9._*+!-]+)*)?
        (\s*;[^@/\\]*)?                                             # markers
        $",
    )
    .expect("a valid pattern")
});

/// What pip reads a bare argument ending in one of these as: a local archive in its working
/// directory, not a name on an index.
const ARCHIVE_SUFFIXES: [&str; 12] = [
    ".whl",
    ".zip",
    ".tar",
    ".tar.gz",
    ".tgz",
    ".tar.bz2",
    ".tbz",
    ".tar.xz",
    ".txz",
    ".tar.lz",
    ".tar.lzma",
    ".tlz",
];

/// The set as it is hashed and installed: each requirement checked to be a specifier, then
/// trimmed, sorted and deduplicated.
fn requirement_set(requires: &[String]) -> Result<Vec<String>> {
    let mut set: Vec<String> = requires.iter().map(|req| req.trim().to_string()).collect();
    // pip reads a bare argument ending in an archive suffix -- its markers aside, and a
    // trailing `[extras]` stripped -- as a file in its working directory, whatever else it
    // looks like: `probe===1.0.tar.gz` is a file to pip.
    let archive = |req: &String| {
        let argument = req.split(';').next().unwrap_or(req).trim_end();
        let argument = match argument
            .strip_suffix(']')
            .and_then(|rest| rest.rsplit_once('['))
        {
            Some((head, _)) => head.trim_end(),
            None => argument,
        };
        let lower = argument.to_ascii_lowercase();
        ARCHIVE_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    };
    if let Some(bad) = set
        .iter()
        .find(|req| !SPECIFIER.is_match(req) || archive(req))
    {
        return Err(OutrigError::Configuration(format!(
            "{bad:?} is not a requirement specifier: a binding's `requires` names distributions \
             resolved from an index -- a name, extras, a version specifier, markers -- and not a \
             path, an archive, a URL or a pip option"
        )));
    }
    set.sort();
    set.dedup();
    Ok(set)
}

/// The directory name for `set`: a digest of the requirement text, so the same set installs
/// once and a changed set installs beside it.
fn hash(set: &[String]) -> String {
    blake3::hash(set.join("\n").as_bytes()).to_hex()[..16].to_string()
}

/// Install `set` into `target`, through a stage beside it that is renamed into place once
/// complete, so `target` either holds a whole install or does not exist.
fn install_into(
    root: &Path,
    target: &Path,
    python: &Path,
    set: &[String],
    env: &[(OsString, OsString)],
) -> Result<()> {
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(root)
        .path_ctx("create a directory in", root)?;
    if !set.is_empty() {
        let report = tempfile::Builder::new()
            .prefix(".install-report-")
            .suffix(".json")
            .tempfile_in(root)
            .path_ctx("create a file in", root)?;
        run_pip(python, stage.path(), report.path(), set, env)?;
        let dists = dists(stage.path())?;
        check_reported(&dists, report.path(), set)?;
        check_pure(stage.path(), &dists, set)?;
    }
    match std::fs::rename(stage.path(), target) {
        Ok(()) => Ok(()),
        Err(_) if target.is_dir() => Ok(()),
        Err(e) => Err(e).path_ctx("move the installed packages to", target),
    }
}

fn run_pip(
    python: &Path,
    stage: &Path,
    report: &Path,
    set: &[String],
    env: &[(OsString, OsString)],
) -> Result<()> {
    let output = Command::new(python)
        .args([
            "-I",
            "-c",
            PIP,
            "install",
            "--only-binary=:all:",
            "--target",
        ])
        .arg(stage)
        .arg("--report")
        .arg(report)
        .args([
            "--disable-pip-version-check",
            "--no-input",
            "--progress-bar",
            "off",
        ])
        .args(set)
        // Settings in the user's pip configuration that would refuse an install into a cache
        // directory, or send it elsewhere -- or nowhere -- with a clean exit and an empty
        // stage, are each overridden; the rest of the configuration stands.
        .env("PIP_REQUIRE_VIRTUALENV", "0")
        .env("PIP_USER", "0")
        .env("PIP_DRY_RUN", "0")
        .env("PIP_ROOT", "/")
        .env("PIP_NO_DEPS", "0")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .path_ctx("run pip with", python)?;
    if output.status.success() {
        return Ok(());
    }
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let unmet = said
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("ERROR: No matching distribution found for "))
        .map(|req| format!("no wheel satisfies {}, and ", req.trim()))
        .unwrap_or_default();
    Err(OutrigError::Configuration(format!(
        "cannot install {}: {unmet}{RULE}; pip ran with --only-binary=:all:, so a requirement \
         that ships only a source distribution cannot be hosted until a wheel of it exists. pip \
         said:\n{}",
        render(set),
        tail_string(said.as_bytes(), PIP_TAIL).trim_end()
    )))
}

/// Refuse a stage missing a distribution pip's report says it installed. A `root` or a `prefix`
/// in the user's pip configuration sends the files elsewhere and a `dry-run` nowhere, each with
/// a clean exit, and an empty stage published once would be served from then on. A stage that
/// is legitimately empty -- every requirement's marker false -- reports no install.
fn check_reported(dists: &[Dist], report: &Path, set: &[String]) -> Result<()> {
    let text = std::fs::read_to_string(report).path_ctx("read pip's report", report)?;
    let report: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        OutrigError::Configuration(format!("pip's report of the install is not JSON: {e}"))
    })?;
    let staged: Vec<String> = dists
        .iter()
        .map(|dist| canonical_name(&dist.name))
        .collect();
    let missing: Vec<String> = report["install"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item["metadata"]["name"].as_str()?;
            let version = item["metadata"]["version"].as_str().unwrap_or("?");
            (!staged.contains(&canonical_name(name))).then(|| format!("{name} {version}"))
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(OutrigError::Configuration(format!(
        "cannot install {}: pip reported installing {} and left none of it in the install \
         directory; a pip setting such as a root, a prefix or a dry run sends an install \
         elsewhere, and the directory would otherwise be served empty from now on",
        render(set),
        missing.join(", ")
    )))
}

/// Refuse anything in `stage` that is not pure Python: a wheel whose tags say otherwise, and an
/// extension module among the files of one whose tags do not.
fn check_pure(stage: &Path, dists: &[Dist], set: &[String]) -> Result<()> {
    for dist in dists {
        let wheel = dist.dir.join("WHEEL");
        let text = std::fs::read_to_string(&wheel).path_ctx("read", &wheel)?;
        let tags: Vec<&str> = text
            .lines()
            .filter_map(|line| line.strip_prefix("Tag:"))
            .map(str::trim)
            .collect();
        // `bdist_wheel` writes a compressed tag such as `py2.py3-none-any` as one `Tag:` line
        // per Python tag, so a wheel's lines are judged together: every one for no ABI and
        // any platform, and one of them for Python 3.
        let pure_wheel = !tags.is_empty()
            && tags.iter().all(|tag| tag.ends_with("-none-any"))
            && tags.iter().any(|tag| pure(tag));
        if !pure_wheel {
            let what = match tags.iter().find(|tag| !pure(tag)) {
                Some(tag) => format!("is tagged {tag}"),
                None => "declares no tag".to_string(),
            };
            return Err(not_pure(set, dist, &what));
        }
    }
    for entry in walkdir::WalkDir::new(stage) {
        let entry = entry.map_err(|e| OutrigError::Io(e.into()))?;
        if !matches!(
            entry.path().extension().and_then(|ext| ext.to_str()),
            Some("so" | "pyd")
        ) {
            continue;
        }
        let file = entry.path().strip_prefix(stage).unwrap_or(entry.path());
        let what = format!("carries the extension module {}", file.display());
        return Err(match owner_of(dists, file)? {
            Some(dist) => not_pure(set, dist, &what),
            None => OutrigError::Configuration(format!(
                "cannot install {}: an unknown distribution {what}, and {RULE}",
                render(set)
            )),
        });
    }
    Ok(())
}

/// An installed distribution, from its `.dist-info` directory.
struct Dist {
    name: String,
    version: String,
    dir: PathBuf,
}

/// Every distribution installed in `stage`.
fn dists(stage: &Path) -> Result<Vec<Dist>> {
    let mut dists = Vec::new();
    for entry in std::fs::read_dir(stage).path_ctx("read", stage)? {
        let entry = entry.path_ctx("read", stage)?;
        let file_name = entry.file_name();
        // `<name>-<version>.dist-info`, with `-` replaced by `_` in both parts.
        let Some((name, version)) = file_name
            .to_str()
            .and_then(|name| name.strip_suffix(".dist-info"))
            .and_then(|stem| stem.split_once('-'))
        else {
            continue;
        };
        dists.push(Dist {
            name: name.to_string(),
            version: version.to_string(),
            dir: entry.path(),
        });
    }
    Ok(dists)
}

/// The distribution whose `RECORD` lists `file`, relative to the stage.
fn owner_of<'a>(dists: &'a [Dist], file: &Path) -> Result<Option<&'a Dist>> {
    let listed = format!("{},", file.display());
    for dist in dists {
        let record = dist.dir.join("RECORD");
        let text = std::fs::read_to_string(&record).path_ctx("read", &record)?;
        if text.lines().any(|line| line.starts_with(&listed)) {
            return Ok(Some(dist));
        }
    }
    Ok(None)
}

/// Whether a wheel tag describes pure Python for Python 3.
fn pure(tag: &str) -> bool {
    let parts: Vec<&str> = tag.split('-').collect();
    let [python, "none", "any"] = parts[..] else {
        return false;
    };
    python
        .split('.')
        .any(|part| part.starts_with("py3") || part.starts_with("cp3"))
}

/// The refusal of `dist`, which `what` describes, naming the requirement of `set` it came from.
fn not_pure(set: &[String], dist: &Dist, what: &str) -> OutrigError {
    let canonical = canonical_name(&dist.name);
    let origin = set
        .iter()
        .find(|req| canonical_name(requirement_name(req)) == canonical)
        .map_or_else(
            || "a dependency of the set".to_string(),
            |req| format!("requirement \"{req}\""),
        );
    OutrigError::Configuration(format!(
        "cannot install {}: the wheel for {} {} ({origin}) {what}, and {RULE}",
        render(set),
        dist.name,
        dist.version
    ))
}

/// The name a requirement specifier starts with.
fn requirement_name(req: &str) -> &str {
    req.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .next()
        .unwrap_or(req)
}

/// A distribution name as pip compares them: lower-case, runs of `-`, `_` and `.` as one `-`.
fn canonical_name(name: &str) -> String {
    name.to_ascii_lowercase()
        .split(['-', '_', '.'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn render(set: &[String]) -> String {
    format!("the requirement set [{}]", set.join(", "))
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use nix::fcntl::{Flock, FlockArg};

    use super::super::testing::{pip_env, python, wheel_links};
    use super::*;

    fn installed(root: &Path, requires: &[&str], env: &[(OsString, OsString)]) -> Result<PathBuf> {
        let requires: Vec<String> = requires.iter().map(|req| req.to_string()).collect();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(install(
                root,
                Install {
                    python: python(),
                    requires: &requires,
                    env,
                },
            ))
    }

    /// `module.ANSWER`, imported from `dir` by the payload's Python.
    fn answer(dir: &Path, module: &str) -> String {
        let output = Command::new(python())
            .args([
                "-I",
                "-c",
                &format!("import sys; sys.path.insert(0, {dir:?}); import {module}; print({module}.ANSWER)"),
            ])
            .output()
            .expect("the payload's Python runs");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// What `root` holds besides lock files.
    fn entries(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(root)
            .expect("read the root")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| !name.ends_with(".lock"))
            .collect();
        names.sort();
        names
    }

    fn message(result: Result<PathBuf>) -> String {
        match result {
            Ok(dir) => panic!("installed into {}", dir.display()),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn a_pure_wheel_installs_and_imports() {
        let root = tempfile::tempdir().expect("a root");
        let dir =
            installed(root.path(), &["pure==1.0"], &pip_env(wheel_links())).expect("installs");
        assert!(dir.starts_with(root.path()));
        assert_eq!(answer(&dir, "pure"), "42\n");
        assert!(dir.join("pure-1.0.dist-info/WHEEL").is_file());
        let name = dir
            .file_name()
            .expect("a name")
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            entries(root.path()),
            [name],
            "nothing but the install is left behind"
        );
        // A second call finds the install: given no wheels at all, pip would have failed.
        let empty = tempfile::tempdir().expect("an empty links directory");
        assert_eq!(
            installed(root.path(), &["pure==1.0"], &pip_env(empty.path())).expect("found"),
            dir
        );
    }

    #[test]
    fn the_bindings_directory_is_under_the_cache_root() {
        let dir = bindings_dir().expect("a cache root");
        assert!(
            dir.is_absolute() && dir.ends_with("outrig/bindings"),
            "{}",
            dir.display()
        );
    }

    /// `bdist_wheel` writes `py2.py3-none-any` as two `Tag:` lines, `py2-none-any` and
    /// `py3-none-any`; judged one by one, the first would refuse every universal wheel.
    #[test]
    fn a_universal_wheel_written_as_two_tag_lines_installs() {
        let root = tempfile::tempdir().expect("a root");
        let dir =
            installed(root.path(), &["universal==1.0"], &pip_env(wheel_links())).expect("installs");
        let wheel = std::fs::read_to_string(dir.join("universal-1.0.dist-info/WHEEL")).unwrap();
        assert!(
            wheel.contains("Tag: py2-none-any\nTag: py3-none-any"),
            "{wheel}"
        );
        assert_eq!(answer(&dir, "universal"), "42\n");
    }

    /// `-I` does not keep pip from reading the user's configuration. `user` makes pip refuse
    /// `--target`; `dry-run` and `root` make it exit cleanly with the stage empty. Each is
    /// overridden, and the rest of the configuration stands.
    #[test]
    fn pip_settings_that_would_refuse_or_misdirect_the_install_are_overridden() {
        let root = tempfile::tempdir().expect("a root");
        let conf = tempfile::tempdir().expect("a configuration directory");
        std::fs::write(
            conf.path().join("pip.conf"),
            format!(
                "[install]\nuser = true\ndry-run = true\nroot = {}\n",
                conf.path().join("elsewhere").display()
            ),
        )
        .unwrap();
        let mut env = pip_env(wheel_links());
        env.retain(|(key, _)| key != "PIP_CONFIG_FILE");
        env.push((
            OsString::from("PIP_CONFIG_FILE"),
            conf.path().join("pip.conf").into_os_string(),
        ));
        let dir = installed(root.path(), &["pure==1.0"], &env).expect("installs");
        assert_eq!(answer(&dir, "pure"), "42\n");
        assert!(!conf.path().join("elsewhere").exists());
    }

    /// A setting that is not overridden and sends the install elsewhere -- here a dry run given
    /// as a variable, which the caller's variables can set -- leaves pip's report naming what it
    /// installed and the stage holding none of it, which is refused rather than published.
    #[test]
    fn an_install_pip_reports_but_the_stage_lacks_is_refused() {
        let root = tempfile::tempdir().expect("a root");
        let mut env = pip_env(wheel_links());
        env.push((OsString::from("PIP_DRY_RUN"), OsString::from("1")));
        let said = message(installed(root.path(), &["pure==1.0"], &env));
        assert!(
            said.contains("pip reported installing pure 1.0 and left none of it"),
            "{said}"
        );
        assert!(entries(root.path()).is_empty(), "{said}");
    }

    /// `--only-binary` constrains what pip resolves from an index. A wheel may declare a
    /// dependency by URL on a source archive, which pip fetches and whose build backend it runs
    /// to read its metadata -- code of the archive's choosing, on the host; measured with the
    /// payload's pip 26.2.1 before this guard, which left the mark. With the run's
    /// source-distribution step disabled the archive is refused, and its backend never runs.
    #[test]
    fn a_source_archive_a_wheel_depends_on_by_url_is_refused_before_it_is_built() {
        let root = tempfile::tempdir().expect("a root");
        let marks = tempfile::tempdir().expect("a directory for the mark");
        let canary = marks.path().join("built");
        let mut env = pip_env(wheel_links());
        env.push((
            OsString::from("OUTRIG_CANARY"),
            canary.clone().into_os_string(),
        ));
        let said = message(installed(root.path(), &["needs_url==1.0"], &env));
        assert!(
            said.contains("evil")
                && said.contains(
                    "is a source distribution, and only pure-Python wheels are installed"
                ),
            "{said}"
        );
        assert!(!canary.exists(), "the archive's build backend ran");
        assert!(entries(root.path()).is_empty(), "{said}");
    }

    /// A requirement whose marker is false installs nothing, and that is a complete install.
    #[test]
    fn a_requirement_whose_marker_is_false_installs_nothing() {
        let root = tempfile::tempdir().expect("a root");
        let dir = installed(
            root.path(),
            &["pure==1.0; python_version < \"3\""],
            &pip_env(wheel_links()),
        )
        .expect("installs nothing");
        assert!(dists(&dir).expect("read").is_empty());
    }

    #[test]
    fn the_hash_is_over_the_set() {
        let a = hash(&requirement_set(&["pure==1.0".into(), "other".into()]).unwrap());
        let b = hash(
            &requirement_set(&[" other ".into(), "pure==1.0".into(), "pure==1.0".into()]).unwrap(),
        );
        let c = hash(&requirement_set(&["pure==1.1".into(), "other".into()]).unwrap());
        assert_eq!(a, b, "order, duplicates and whitespace do not count");
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn an_empty_set_is_an_empty_directory() {
        let root = tempfile::tempdir().expect("a root");
        let dir = installed(root.path(), &[], &[]).expect("installs nothing");
        assert!(dir.is_dir());
        assert_eq!(std::fs::read_dir(&dir).expect("read").count(), 0);
    }

    /// An install that waited on the lock finds the one another finished meanwhile, and does
    /// not run pip: the waiter is given no wheels at all, so had it installed it would have
    /// failed -- where a real second install would merely lose the rename and pass unnoticed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_installs_started_together_install_once_and_both_import() {
        let root = tempfile::tempdir().expect("a root");
        let set = requirement_set(&["pure==1.0".into()]).unwrap();
        let dir = root.path().join(hash(&set));
        std::fs::create_dir_all(root.path()).unwrap();
        let lock = std::fs::File::create(payload::lock_path(&dir)).unwrap();
        let held = Flock::lock(lock, FlockArg::LockExclusive).unwrap();

        let empty = tempfile::tempdir().expect("an empty links directory");
        let waiter = {
            let (root, env) = (root.path().to_path_buf(), pip_env(empty.path()));
            tokio::spawn(async move {
                install(
                    &root,
                    Install {
                        python: python(),
                        requires: &["pure==1.0".to_string()],
                        env: &env,
                    },
                )
                .await
            })
        };
        // The first session's install, done while the waiter holds for the lock.
        let real = pip_env(wheel_links());
        install_into(root.path(), &dir, python(), &set, &real).expect("the first install");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!waiter.is_finished(), "the waiter waited for the lock");
        drop(held);

        let found = waiter
            .await
            .expect("the waiter ran")
            .expect("the waiter found the install");
        assert_eq!(found, dir);
        assert_eq!(answer(&dir, "pure"), "42\n");
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(entries(root.path()), [name]);
    }

    #[test]
    fn a_compiled_wheel_pip_would_install_is_refused_by_the_tag_check() {
        let root = tempfile::tempdir().expect("a root");
        let said = message(installed(
            root.path(),
            &["compiled==1.0"],
            &pip_env(wheel_links()),
        ));
        assert!(
            said.contains(
                "the wheel for compiled 1.0 (requirement \"compiled==1.0\") is tagged cp3"
            ),
            "{said}"
        );
        assert!(
            said.contains("-linux_") && said.contains("only pure-Python wheels are installed"),
            "{said}"
        );
        assert!(
            entries(root.path()).is_empty(),
            "nothing is left in the cache: {said}"
        );
    }

    #[test]
    fn a_requirement_shipping_only_an_sdist_is_refused_by_only_binary() {
        let root = tempfile::tempdir().expect("a root");
        let said = message(installed(
            root.path(),
            &["sdistonly==1.0"],
            &pip_env(wheel_links()),
        ));
        assert!(said.contains("no wheel satisfies sdistonly==1.0"), "{said}");
        assert!(
            said.contains("only pure-Python wheels are installed")
                && said.contains("--only-binary=:all:"),
            "{said}"
        );
        assert!(
            entries(root.path()).is_empty(),
            "nothing is left in the cache: {said}"
        );
    }

    #[test]
    fn an_extension_module_inside_a_pure_tagged_wheel_is_refused() {
        let root = tempfile::tempdir().expect("a root");
        let said = message(installed(
            root.path(),
            &["mislabeled==1.0"],
            &pip_env(wheel_links()),
        ));
        assert!(said.contains("the wheel for mislabeled 1.0 (requirement \"mislabeled==1.0\") carries the extension module mislabeled_ext.so"), "{said}");
        assert!(entries(root.path()).is_empty(), "{said}");
    }

    #[test]
    fn a_compiled_dependency_is_refused_as_a_dependency_of_the_set() {
        let root = tempfile::tempdir().expect("a root");
        let said = message(installed(
            root.path(),
            &["needs_compiled==1.0"],
            &pip_env(wheel_links()),
        ));
        assert!(
            said.contains("the wheel for compiled 1.0 (a dependency of the set) is tagged"),
            "{said}"
        );
        assert!(entries(root.path()).is_empty(), "{said}");
    }

    #[test]
    fn a_path_url_or_option_is_refused_before_pip_runs() {
        let root = tempfile::tempdir().expect("a root");
        for bad in [
            "./pure-1.0-py3-none-any.whl",
            "/tmp/pure-1.0-py3-none-any.whl",
            "~/pure",
            "https://example.com/pure-1.0-py3-none-any.whl",
            "pure @ https://example.com/pure-1.0-py3-none-any.whl",
            "-e .",
            "--index-url=https://example.com/simple",
            "-r requirements.txt",
            "foo==",
            "foo==1.0 --hash=sha256:abc",
            "foo==1.0 bar",
            // Bare archive arguments, which pip reads as files in its working directory,
            // whatever else they look like.
            "pure-1.0-py3-none-any.whl",
            "Pure-1.0-py3-none-any.WHL",
            "pure-1.0.tar.gz",
            "pure-1.0-py3-none-any.whl; python_version > \"3\"",
            "probe===1.0.tar.gz",
            "probe==1.0.whl",
            "probe === 1.0.zip ; python_version > \"3\"",
            "probe-1.0.tar.gz[extra]",
            "",
        ] {
            let requires = [bad.to_string()];
            // pip is never reached: its interpreter does not exist.
            let result = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(install(
                    root.path(),
                    Install {
                        python: Path::new("/nonexistent/python3"),
                        requires: &requires,
                        env: &[],
                    },
                ));
            let said = message(result);
            assert!(
                said.contains("is not a requirement specifier"),
                "{bad:?}: {said}"
            );
        }
        assert!(entries(root.path()).is_empty());
        for good in [
            "GitPython==3.2.0",
            "gitdb<5,>=4.0.1",
            "requests[socks] >= 2.0",
            "typing-extensions>=3.10.0.2; python_version < \"3.10\"",
            "Some_Name.v2",
        ] {
            requirement_set(&[good.to_string()]).unwrap_or_else(|e| panic!("{good:?}: {e}"));
        }
    }

    #[test]
    fn the_pure_tags_are_those_with_platform_any_abi_none_and_python_3() {
        for tag in [
            "py3-none-any",
            "py2.py3-none-any",
            "cp313-none-any",
            "py30-none-any",
        ] {
            assert!(pure(tag), "{tag}");
        }
        for tag in [
            "cp313-cp313-linux_x86_64",
            "cp313-abi3-manylinux_2_17_x86_64",
            "py3-none-linux_x86_64",
            "py2-none-any",
            "py3-cp313-any",
            "py3-none",
            "",
        ] {
            assert!(!pure(tag), "{tag}");
        }
    }
}
