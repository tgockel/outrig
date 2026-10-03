//! CLI-owned repo, config, and cache path policy.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use tempfile::NamedTempFile;

use outrig::config::Config;
use outrig::error::{IoPathExt, OutrigError, Result};

const REPO_CONFIG_REL: &str = ".agents/outrig/config.toml";
const IMAGES_REL: &str = ".agents/outrig/images";
const GLOBAL_CONFIG_FILE: &str = "config.toml";
const GLOBAL_HOME_DIR: &str = ".outrig";
const GLOBAL_XDG_DIR: &str = "outrig";

/// The process working directory, with a message that says what went wrong.
/// The bare `std::env::current_dir()` error is an unqualified ENOENT --
/// indistinguishable from a missing file -- and it fires whenever the
/// directory the user launched from has since been deleted or unmounted.
pub(crate) fn current_dir() -> Result<PathBuf> {
    std::env::current_dir().map_err(|source| {
        OutrigError::Configuration(format!(
            "cannot determine the current directory: {source}\n\
             help: the directory may have been deleted or unmounted -- `cd` somewhere else and retry"
        ))
    })
}

pub(crate) fn find_repo_root_from(cwd: &Path) -> Result<PathBuf> {
    let mut cur = cwd;
    loop {
        if cur.join(REPO_CONFIG_REL).is_file() {
            return Ok(cur.to_path_buf());
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return Err(OutrigError::NoRepoConfig),
        }
    }
}

pub(crate) fn repo_config_path(root: &Path) -> PathBuf {
    root.join(REPO_CONFIG_REL)
}

pub(crate) fn image_dir(root: &Path, name: &str) -> PathBuf {
    root.join(IMAGES_REL).join(name)
}

pub(crate) fn image_dir_rel(name: &str) -> PathBuf {
    Path::new(IMAGES_REL).join(name)
}

pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    write_atomic_all(&[(path, contents)])
}

/// Writes each file atomically, in order, after staging every one in a temp
/// file beside its target: creating, writing, or syncing any of them fails
/// before a single target changes. Only a failed rename can leave the
/// earlier files written and the later ones not.
pub(crate) fn write_atomic_all(files: &[(&Path, &str)]) -> Result<()> {
    let staged = files
        .iter()
        .map(|&(path, contents)| stage(path, contents).map(|tmp| (tmp, path)))
        .collect::<Result<Vec<_>>>()?;
    for (tmp, path) in staged {
        tmp.persist(path).map_err(OutrigError::from)?;
    }
    Ok(())
}

/// `contents` in a synced temp file in `path`'s directory, created if
/// missing. Dropped instead of persisted, the temp file is removed.
fn stage(path: &Path, contents: &str) -> Result<NamedTempFile> {
    let parent = path.parent().ok_or_else(|| {
        OutrigError::Configuration(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).path_ctx("create directory", parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    Ok(tmp)
}

/// The repo config a command reads, and the repo it runs against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoConfig {
    /// The repo: the default workspace, the session's working directory, and
    /// what `model-path` resolves against.
    pub root: PathBuf,
    /// A `--config` file outside `.agents/outrig/`, read in place of the
    /// repo's own. `None` reads `<root>/.agents/outrig/config.toml`, which a
    /// config-less `outrig run` or `outrig mcp` does not have.
    pub file: Option<PathBuf>,
}

impl RepoConfig {
    /// The repo at `root`, reading its own `.agents/outrig/config.toml`.
    pub fn at_root(root: PathBuf) -> Self {
        Self { root, file: None }
    }

    /// The file this reads.
    pub(crate) fn config_path(&self) -> PathBuf {
        self.file
            .clone()
            .unwrap_or_else(|| repo_config_path(&self.root))
    }

    pub(crate) fn load(&self, global: &Path) -> Result<Config> {
        match &self.file {
            Some(file) => Config::load_file(file, &self.root, Some(global)),
            None => Config::load(&self.root, Some(global)),
        }
    }

    pub(crate) fn load_for_run(
        &self,
        global: &Path,
        agent_flag: Option<&str>,
        model_override: Option<&str>,
    ) -> Result<Config> {
        match &self.file {
            Some(file) => Config::load_file_for_run(
                file,
                &self.root,
                Some(global),
                agent_flag,
                model_override,
            ),
            None => Config::load_for_run(&self.root, Some(global), agent_flag, model_override),
        }
    }

    pub(crate) fn load_for_build(&self, global: &Path) -> Result<Config> {
        match &self.file {
            Some(file) => Config::load_file_for_build(file, &self.root, Some(global)),
            None => Config::load_for_build(&self.root, Some(global)),
        }
    }
}

/// The repo config for `ls`/`logs`/`discard`/`clean`/`build`: `--config`
/// when given (see [`explicit_repo_config`]), else the walk up from `cwd`,
/// where finding nothing is [`OutrigError::NoRepoConfig`].
pub(crate) fn resolve_repo_config(override_path: Option<&Path>, cwd: &Path) -> Result<RepoConfig> {
    match override_path {
        Some(p) => explicit_repo_config(p, cwd),
        None => find_repo_root_from(cwd).map(RepoConfig::at_root),
    }
}

/// Like [`resolve_repo_config`] but finding no repo config is not an error.
/// `outrig run`/`outrig mcp` may run in a directory with no
/// `.agents/outrig/config.toml`. With no `--config` and nothing found up the
/// tree, `cwd` is the root, and `Config::load` treats its missing file as an
/// empty config merged over the global config.
pub(crate) fn resolve_repo_config_optional(
    override_path: Option<&Path>,
    cwd: &Path,
) -> Result<RepoConfig> {
    match override_path {
        Some(p) => explicit_repo_config(p, cwd),
        None => Ok(RepoConfig::at_root(repo_root_or_cwd(cwd))),
    }
}

/// `--config <path>`. The file has to exist: the flag names it outright, so
/// standing in an empty config would run a session it does not describe.
/// One at `<repo>/.agents/outrig/config.toml` is that repo's own, and means
/// what running from `<repo>` means. Any other is read on its own, for the
/// repo found from `cwd` as if no flag were given: where the file sits never
/// picks the directory mounted as the workspace.
///
/// The path is taken against `cwd` first, so the root is absolute. Taken as
/// written, `--config .agents/outrig/config.toml` has the empty path three
/// levels up, and the empty path is not the current directory to what
/// receives it: `Path::new("").exists()` is false, and mistralrs-core reads an
/// empty model directory as a Hugging Face repo ID.
fn explicit_repo_config(path: &Path, cwd: &Path) -> Result<RepoConfig> {
    let file: PathBuf = cwd.join(path).components().collect();
    if !file.is_file() {
        return Err(OutrigError::Configuration(format!(
            "--config {} is not an existing file",
            path.display()
        )));
    }
    if file.ends_with(REPO_CONFIG_REL) {
        let root = file.ancestors().nth(3).expect("an absolute path ending in \
            .agents/outrig/config.toml has a third ancestor");
        return Ok(RepoConfig::at_root(root.to_path_buf()));
    }
    Ok(RepoConfig {
        root: repo_root_or_cwd(cwd),
        file: Some(file),
    })
}

fn repo_root_or_cwd(cwd: &Path) -> PathBuf {
    find_repo_root_from(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

/// Build context for one of outrig's built-in images, under the user cache
/// directory. A subdirectory per image: the content hash covers the whole
/// context, so two built-ins sharing a directory would bust each other's tag.
pub(crate) fn builtin_image_dir(name: &str) -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.cache_dir().join("builtin-images").join(name);
    }
    std::env::temp_dir()
        .join("outrig-builtin-images")
        .join(name)
}

pub(crate) fn model_cache_root(from_config: Option<&Path>) -> PathBuf {
    if let Some(p) = from_config {
        return p.to_path_buf();
    }
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.cache_dir().join("models");
    }
    std::env::temp_dir().join("outrig-models")
}

pub(crate) fn default_session_root() -> PathBuf {
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.data_dir().join("sessions");
    }
    std::env::temp_dir().join("outrig-sessions")
}

pub(crate) fn global_config_path(override_path: Option<&Path>) -> PathBuf {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_default();
    global_config_path_with(override_path, xdg.as_deref(), &home)
}

fn global_config_path_with(
    override_path: Option<&Path>,
    xdg_config_home: Option<&Path>,
    home: &Path,
) -> PathBuf {
    if let Some(p) = override_path {
        return p.to_path_buf();
    }
    if let Some(xdg) = xdg_config_home {
        return xdg.join(GLOBAL_XDG_DIR).join(GLOBAL_CONFIG_FILE);
    }
    home.join(GLOBAL_HOME_DIR).join(GLOBAL_CONFIG_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::tempdir;

    fn write_repo_config(root: &Path) -> PathBuf {
        let agents = root.join(".agents").join("outrig");
        fs::create_dir_all(&agents).unwrap();
        let config = agents.join("config.toml");
        fs::write(&config, b"# fixture\n").unwrap();
        config
    }

    /// A file that can't be staged fails the write before any file changes,
    /// the ones listed ahead of it included, and leaves no temp file behind.
    #[test]
    fn write_atomic_all_changes_nothing_when_a_file_cannot_be_staged() {
        let tmp = tempdir().unwrap();
        let first = tmp.path().join("first");
        fs::write(&first, "old").unwrap();
        // A regular file where the second target's directory would go.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, "").unwrap();

        write_atomic_all(&[(&first, "new"), (&blocker.join("second"), "new")])
            .expect_err("the second file's directory can't be created");

        assert_eq!(fs::read_to_string(&first).unwrap(), "old");
        let mut left: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left, ["blocker", "first"]);
    }

    #[test]
    fn find_repo_root_missing_returns_documented_error() {
        let tmp = tempdir().unwrap();
        let err = find_repo_root_from(tmp.path()).unwrap_err();
        assert!(matches!(err, OutrigError::NoRepoConfig));
        assert_eq!(
            err.to_string(),
            "no .agents/outrig/config.toml found in current directory or any parent\n\
             help: run `outrig init` to initialize",
        );
    }

    #[test]
    fn find_repo_root_from_nested_cwd_finds_parent_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());
        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn find_repo_root_from_root_dir_finds_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());

        let root = find_repo_root_from(tmp.path()).unwrap();
        assert_eq!(root, tmp.path());
        assert_eq!(
            repo_config_path(&root),
            tmp.path().join(".agents/outrig/config.toml"),
        );
    }

    #[test]
    fn find_repo_root_skips_agents_dir_without_outrig_config() {
        let tmp = tempdir().unwrap();
        write_repo_config(tmp.path());

        let bare = tmp.path().join("a");
        fs::create_dir_all(bare.join(".agents")).unwrap();
        let nested = bare.join("b/c");
        fs::create_dir_all(&nested).unwrap();

        let root = find_repo_root_from(&nested).unwrap();
        assert_eq!(root, tmp.path());
    }

    /// `--config <repo>/.agents/outrig/config.toml` is that repo, wherever the
    /// command runs from: the documented way to point an MCP client at a repo.
    #[test]
    fn explicit_repo_config_is_its_own_repo() {
        let repo = tempdir().unwrap();
        let config = write_repo_config(repo.path());
        let elsewhere = tempdir().unwrap();

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&config), elsewhere.path()).unwrap();
            assert_eq!(resolved, RepoConfig::at_root(repo.path().to_path_buf()));
            assert_eq!(resolved.config_path(), config);
        }
    }

    /// Any other `--config` file is read in place of the repo's own, and the
    /// repo is the one found from the working directory -- not the directory
    /// three levels above the file, which is where #323's `$HOME` came from.
    #[test]
    fn explicit_other_file_is_read_for_the_repo_found_from_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        let nested = repo.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let outside = tempdir().unwrap();
        let file = outside.path().join("deep/ci/outrig.toml");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"# fixture\n").unwrap();
        // Where the old derivation looked: three levels above the file.
        write_repo_config(outside.path());

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&file), &nested).unwrap();
            assert_eq!(
                resolved,
                RepoConfig {
                    root: repo.path().to_path_buf(),
                    file: Some(file.clone()),
                },
            );
            assert_eq!(resolved.config_path(), file);
        }
    }

    /// Outside any repo, an out-of-tree `--config` runs against the working
    /// directory, as a config-less run would.
    #[test]
    fn explicit_other_file_outside_a_repo_runs_against_cwd() {
        let cwd = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let file = outside.path().join("outrig.toml");
        fs::write(&file, b"# fixture\n").unwrap();

        for resolve in [resolve_repo_config, resolve_repo_config_optional] {
            let resolved = resolve(Some(&file), cwd.path()).unwrap();
            assert_eq!(resolved.root, cwd.path());
            assert_eq!(resolved.file.as_deref(), Some(file.as_path()));
        }
    }

    /// An explicit `--config` names a file outright, so one that is not there
    /// -- a typo, or a directory -- stops the command and says which, in
    /// either shape and even with a repo config to fall back to.
    #[test]
    fn explicit_config_that_is_not_a_file_is_refused() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        for missing in [
            repo.path().join(".agents/outrig/confg.toml"),
            repo.path().join("sub/.agents/outrig/config.toml"),
            repo.path().join("ci/outrig.toml"),
            repo.path().join(".agents/outrig"),
        ] {
            for resolve in [resolve_repo_config, resolve_repo_config_optional] {
                let err = resolve(Some(&missing), repo.path()).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    format!(
                        "configuration: --config {} is not an existing file",
                        missing.display()
                    ),
                );
            }
        }
    }

    /// Without the flag, nothing changes: the walk up decides, and a run or
    /// mcp session with nothing to find is config-less in the working
    /// directory.
    #[test]
    fn no_flag_walks_up_or_falls_back_to_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        let nested = repo.path().join("a");
        fs::create_dir_all(&nested).unwrap();
        let bare = tempdir().unwrap();

        let expected = RepoConfig::at_root(repo.path().to_path_buf());
        assert_eq!(resolve_repo_config(None, &nested).unwrap(), expected);
        assert_eq!(resolve_repo_config_optional(None, &nested).unwrap(), expected);
        assert!(matches!(
            resolve_repo_config(None, bare.path()),
            Err(OutrigError::NoRepoConfig)
        ));
        assert_eq!(
            resolve_repo_config_optional(None, bare.path()).unwrap(),
            RepoConfig::at_root(bare.path().to_path_buf()),
        );
    }

    /// A relative `--config` is taken against the working directory, so the
    /// root is absolute in every shape: never the empty path three levels
    /// above `.agents/outrig/config.toml`, which mistralrs-core would read as
    /// a Hugging Face repo ID for a bare `model-path`.
    #[test]
    fn relative_explicit_config_resolves_against_cwd() {
        let repo = tempdir().unwrap();
        write_repo_config(repo.path());
        fs::create_dir_all(repo.path().join("ci")).unwrap();
        fs::write(repo.path().join("ci/outrig.toml"), b"# fixture\n").unwrap();

        for canonical in [".agents/outrig/config.toml", "./.agents/outrig/config.toml"] {
            let resolved = resolve_repo_config(Some(Path::new(canonical)), repo.path()).unwrap();
            assert_eq!(resolved.root, repo.path(), "--config {canonical}");
            assert_eq!(resolved.root.as_os_str(), repo.path().as_os_str());
        }
        let resolved = resolve_repo_config(Some(Path::new("ci/outrig.toml")), repo.path()).unwrap();
        assert_eq!(resolved.file, Some(repo.path().join("ci/outrig.toml")));
    }

    #[test]
    fn global_config_path_xdg_overrides_home_default() {
        let xdg = Path::new("/xdg/conf");
        let home = Path::new("/home/alice");
        let p = global_config_path_with(None, Some(xdg), home);
        assert_eq!(p, Path::new("/xdg/conf/outrig/config.toml"));
    }

    #[test]
    fn global_config_path_flag_overrides_xdg() {
        let flag = Path::new("/explicit/global.toml");
        let xdg = Path::new("/xdg/conf");
        let home = Path::new("/home/alice");
        let p = global_config_path_with(Some(flag), Some(xdg), home);
        assert_eq!(p, flag);
    }

    #[test]
    fn global_config_path_home_default_when_xdg_unset() {
        let home = Path::new("/home/alice");
        let p = global_config_path_with(None, None, home);
        assert_eq!(p, Path::new("/home/alice/.outrig/config.toml"));
    }
}
