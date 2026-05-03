//! Repo discovery and `.agents/outrig/` path resolution.

use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};

use crate::error::{OutrigError, Result};

const REPO_CONFIG_REL: &str = ".agents/outrig/config.toml";
const GLOBAL_CONFIG_FILE: &str = "config.toml";
const GLOBAL_HOME_DIR: &str = ".outrig";
const GLOBAL_XDG_DIR: &str = "outrig";

pub fn find_repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    find_repo_root_from(&cwd)
}

pub fn find_repo_root_from(cwd: &Path) -> Result<PathBuf> {
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

pub fn repo_config_path(root: &Path) -> PathBuf {
    root.join(REPO_CONFIG_REL)
}

/// Inverse of [`repo_config_path`]: given the path
/// `<root>/.agents/outrig/config.toml`, return `<root>`. Falls back to `.`
/// only if the path doesn't have three parents (which shouldn't happen for
/// any path produced by [`resolve_repo_config`]).
pub fn repo_root_from_config_path(repo_cfg: &Path) -> PathBuf {
    repo_cfg
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn resolve_repo_config(override_path: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => find_repo_root_from(cwd).map(|root| repo_config_path(&root)),
    }
}

/// Resolve the directory under which mistralrs (and other LLM backends) stage
/// downloaded model files. CLI override > config's `model-cache-root` > XDG
/// project-dir cache. The XDG fallback uses `directories::ProjectDirs` so it
/// matches platform conventions (`$XDG_CACHE_HOME/outrig/models` on Linux,
/// `~/Library/Caches/outrig/models` on macOS, etc.). Falls back to a temp
/// directory only if the platform can't supply a project-dir at all.
pub fn model_cache_root(from_config: Option<&Path>) -> PathBuf {
    if let Some(p) = from_config {
        return p.to_path_buf();
    }
    if let Some(dirs) = ProjectDirs::from("", "", "outrig") {
        return dirs.cache_dir().join("models");
    }
    std::env::temp_dir().join("outrig-models")
}

pub fn global_config_path(override_path: Option<&Path>) -> PathBuf {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_default();
    global_config_path_with(override_path, xdg.as_deref(), &home)
}

#[doc(hidden)]
pub fn global_config_path_with(
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
