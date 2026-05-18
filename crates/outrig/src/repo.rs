//! Repo discovery needed by `load_project` and `Config::load`.

use std::path::{Path, PathBuf};

use crate::error::{OutrigError, Result};

const REPO_CONFIG_REL: &str = ".agents/outrig/config.toml";

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
