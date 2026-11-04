//! `SessionStore::create` with a relative `--session-dir` or `--session-root`
//! that does not exist yet -- a bare name with no directory of its own, like
//! `new-run`, whose only existing ancestor is the current directory itself.
//! Caught in review of #429: `canonicalize_prefix` walked a relative path's
//! parents without ever anchoring to the cwd, so such a path's walk reached
//! the empty parent `Path::new("new-run").parent() == Some("")` gives, which
//! `fs::canonicalize` cannot resolve and which names no `file_name` to climb
//! past either -- aborting creation with `ENOENT` before the fix below.
//!
//! Alone in its own test binary, and a single test within it, because both
//! cases move the current directory, which every other test sharing the
//! process would see; run and mcp pass paths exactly this shape unnormalized,
//! so this is what either hitting the bug looks like from the top.

use std::path::{Path, PathBuf};

use outrig_cli::session::{SessionId, SessionStore};

mod common;
use common::sample_session;

#[test]
fn an_absent_bare_relative_session_dir_and_root_are_both_accepted() {
    let restore = std::env::current_dir().expect("cwd");

    // A bare relative `--session-dir` that does not exist yet, with an
    // absolute (and already-existing) root -- the `run`/`mcp` common case.
    {
        let cwd = tempfile::tempdir().expect("tempdir cwd");
        std::env::set_current_dir(cwd.path()).expect("chdir");
        let root = tempfile::tempdir().expect("tempdir root");
        let store = SessionStore::new(root.path().to_path_buf());
        let sid = SessionId("20261005T200230-5c0d".into());

        let result = store.create(&sid, Some(Path::new("new-run")), &mut sample_session(&sid));
        std::env::set_current_dir(&restore).expect("restore cwd");

        let (dir, _lock) =
            result.expect("a bare relative --session-dir that does not exist yet is accepted");
        let canon_cwd = cwd.path().canonicalize().expect("canon cwd");
        assert_eq!(dir, canon_cwd.join("new-run"));
        assert!(dir.join("session.json").exists());
    }

    // A bare relative `--session-root` that does not exist yet, given an
    // explicit (absolute) `--session-dir` outside it.
    {
        let cwd = tempfile::tempdir().expect("tempdir cwd");
        std::env::set_current_dir(cwd.path()).expect("chdir");
        let explicit = tempfile::tempdir().expect("tempdir explicit");
        let store = SessionStore::new(PathBuf::from("sessions"));
        let sid = SessionId("20261005T200231-6e1d".into());

        let result = store.create(&sid, Some(explicit.path()), &mut sample_session(&sid));
        std::env::set_current_dir(&restore).expect("restore cwd");

        result.expect("a bare relative --session-root that does not exist yet is accepted");
    }
}
