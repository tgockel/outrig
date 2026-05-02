//! Integration tests for repo and global config path resolution.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::tempdir;

use outrig::error::OutrigError;
use outrig::repo::{
    find_repo_root_from, global_config_path_with, repo_config_path, resolve_repo_config,
};

fn write_repo_config(root: &Path) -> PathBuf {
    let agents = root.join(".agents").join("outrig");
    fs::create_dir_all(&agents).unwrap();
    let config = agents.join("config.toml");
    fs::write(&config, b"# fixture\n").unwrap();
    config
}

mod repo_paths {
    use super::*;

    #[test]
    fn find_repo_root_missing_returns_documented_error() {
        let tmp = tempdir().unwrap();
        let err = find_repo_root_from(tmp.path()).unwrap_err();
        assert!(matches!(err, OutrigError::NoRepoConfig));
        assert_eq!(
            err.to_string(),
            "no .agents/outrig/config.toml found in current directory or any parent",
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
        // An intermediate directory has `.agents/` but no `outrig/config.toml` inside.
        // The walk-up should keep going past it and find the real config above.
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
        // No `.agents/outrig/config.toml` fixture here; the walk-up would fail.
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
