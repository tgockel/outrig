//! Repo discovery and `.agents/outrig/` path resolution.

use std::path::{Path, PathBuf};

use directories::BaseDirs;

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

pub fn resolve_repo_config(override_path: Option<&Path>, cwd: &Path) -> Result<PathBuf> {
    match override_path {
        Some(p) => Ok(p.to_path_buf()),
        None => find_repo_root_from(cwd).map(|root| repo_config_path(&root)),
    }
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
