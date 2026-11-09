//! Repo discovery needed by `load_project` and `Config::load`.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use crate::error::{IoPathExt, OutrigError, Result};

const REPO_CONFIG_REL: &str = ".agents/outrig/config.toml";

pub(crate) fn find_repo_root_from(cwd: &Path) -> Result<PathBuf> {
    find_repo_root_with(cwd, nix::unistd::geteuid().as_raw())
}

/// [`find_repo_root_from`] for the user `uid`.
fn find_repo_root_with(cwd: &Path, uid: u32) -> Result<PathBuf> {
    let mut cur = cwd;
    loop {
        let config = cur.join(REPO_CONFIG_REL);
        match fs::metadata(&config) {
            Ok(meta) if meta.is_file() => {
                refuse_foreign_repo_config(cur, uid)?;
                return Ok(cur.to_path_buf());
            }
            // A `.agents/` or `.agents/outrig/` that is a file rather than a
            // directory, or a `config.toml` that is a directory: not the
            // config this is looking for, so the walk goes on, same as a
            // missing entry.
            Ok(_) => {}
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {}
            // Anything else says nothing about whether the config is there,
            // so it is not treated as absent (#829; see outrig-cli's
            // `paths::find_repo_root_with`, which has the same split).
            Err(e) => return Err(e).path_ctx("stat", config),
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return Err(OutrigError::NoRepoConfig),
        }
    }
}

/// Refuse the repo config under `root` unless `uid` owns it, the
/// `.agents/outrig/` directories it sits in, and `root` itself: whoever owns
/// any of those could have put it there. outrig-cli's walk applies the same
/// rule, and says why at length.
fn refuse_foreign_repo_config(root: &Path, uid: u32) -> Result<()> {
    // A walk from a relative `dir` can end at the empty path, which is the
    // current directory to `join` but nothing at all to `stat`.
    let root = if root.as_os_str().is_empty() {
        Path::new(".")
    } else {
        root
    };
    let config = repo_config_path(root);
    // `config.toml`, `outrig/`, `.agents/`, and then `root`.
    for path in config.ancestors().take(4) {
        let owner = std::fs::symlink_metadata(path)
            .path_ctx("stat", path)?
            .uid();
        if owner != uid {
            let entry = if path == config {
                "it".to_string()
            } else {
                path.display().to_string()
            };
            return Err(OutrigError::Configuration(format!(
                "refusing {config}: {entry} is owned by uid {owner}, not by this process's \
                 uid {uid}, so another user could have written it",
                config = config.display(),
            )));
        }
    }
    Ok(())
}

pub(crate) fn repo_config_path(root: &Path) -> PathBuf {
    root.join(REPO_CONFIG_REL)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// #829: `.agents/outrig/` the caller cannot search must stop the walk
    /// rather than be read as absent -- a config further up, or none, picked
    /// silently in place of one never examined.
    #[test]
    fn a_config_that_cannot_be_examined_is_reported_not_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let agents = proj.join(".agents").join("outrig");
        fs::create_dir_all(&agents).unwrap();
        let config = agents.join("config.toml");
        fs::write(&config, "").unwrap();
        let nested = proj.join("sub");
        fs::create_dir_all(&nested).unwrap();
        let uid = fs::metadata(&config).unwrap().uid();
        fs::set_permissions(&agents, fs::Permissions::from_mode(0o000)).unwrap();
        // A superuser searches any directory, so there is nothing to see.
        let searchable = fs::metadata(&config).is_ok();

        let result = find_repo_root_with(&nested, uid);

        fs::set_permissions(&agents, fs::Permissions::from_mode(0o755)).unwrap();
        if searchable {
            return;
        }
        let err = result.unwrap_err();
        assert!(
            matches!(&err, OutrigError::Path { op: "stat", path, .. } if *path == config),
            "{err:?}"
        );
    }

    /// A config another user owns is refused by name, not taken and not
    /// walked past.
    #[test]
    fn a_config_another_user_owns_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let config = repo_config_path(tmp.path());
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "").unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        let owner = fs::metadata(&config).unwrap().uid();

        assert_eq!(find_repo_root_with(&nested, owner).unwrap(), tmp.path());

        let err = find_repo_root_with(&nested, owner + 1).unwrap_err();
        assert!(
            matches!(&err, OutrigError::Configuration(msg)
                if msg.starts_with(&format!("refusing {}:", config.display()))),
            "{err:?}"
        );
    }
}
