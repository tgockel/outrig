//! `load_project` from a relative directory. Walking up from `src` ends at
//! the empty path -- the process's current directory -- and a config there
//! is found and loaded as it always was, its owner checked like any other.
//!
//! Alone in its own test binary because it moves the current directory,
//! which every other test sharing the process would see.

use std::fs;
use std::path::Path;

#[test]
fn a_relative_dir_finds_the_config_in_the_current_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join(".agents/outrig/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "").unwrap();
    fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();

    let (_, root) = outrig::load_project(Path::new("src"), None).unwrap();
    assert_eq!(root, Path::new(""));
}
