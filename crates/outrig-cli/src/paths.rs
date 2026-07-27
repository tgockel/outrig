//! CLI-owned repo, config, and cache path policy.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};
use tempfile::NamedTempFile;

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
    let parent = path.parent().ok_or_else(|| {
        OutrigError::Configuration(format!("path has no parent: {}", path.display()))
    })?;
    std::fs::create_dir_all(parent).path_ctx("create directory", parent)?;
    let mut tmp = NamedTempFile::new_in(parent)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(OutrigError::from)?;
    Ok(())
}

pub(crate) fn repo_root_from_config_path(repo_cfg: &Path) -> PathBuf {
    repo_cfg
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub(crate) fn resolve_repo_config(override_path: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => find_repo_root_from(cwd).map(|root| repo_config_path(&root)),
    }
}

/// Like [`resolve_repo_config`] but never fails when no repo config is found.
/// `outrig run`/`outrig mcp` may run in a directory with no
/// `.agents/outrig/config.toml`. With no `--config` override and nothing found
/// up the tree, synthesize `<cwd>/.agents/outrig/config.toml` -- a path whose
/// file is absent. [`repo_root_from_config_path`] maps it back to `cwd`, and
/// `Config::load` treats the missing file as an empty config merged over the
/// global config.
pub(crate) fn resolve_repo_config_optional(override_path: Option<&Path>, cwd: &Path) -> PathBuf {
    match override_path {
        Some(p) => p.to_path_buf(),
        None => {
            let root = find_repo_root_from(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            repo_config_path(&root)
        }
    }
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

    #[test]
    fn resolve_repo_config_override_skips_walk_up() {
        let tmp = tempdir().unwrap();
        let custom = tmp.path().join("elsewhere/my-config.toml");
        let resolved = resolve_repo_config(Some(&custom), tmp.path()).unwrap();
        assert_eq!(resolved, custom);
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
