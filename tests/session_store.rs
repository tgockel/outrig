//! SessionStore on-disk behavior. Each test owns its own tempdir as the
//! session root; symlinks land inside that tree so cleanup is automatic.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use outrig::error::OutrigError;
use outrig::session::{Session, SessionId, SessionStore};

fn sample_session(id: &SessionId) -> Session {
    Session {
        id: id.clone(),
        started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        ended_at: None,
        container_name: format!("outrig-{}", id.as_str()),
        image_tag: "outrig/test:abc123".to_string(),
        container_config_name: "coding".to_string(),
        agent_name: "default".to_string(),
        working_dir: PathBuf::from("/some/repo"),
        session_dir: PathBuf::new(),
        exit_code: None,
        link_target: None,
    }
}

#[test]
fn auto_path_creates_session_json() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = root.path().join(sid.as_str());
    let mut session = sample_session(&sid);

    let returned = store.create(&sid, None, &mut session).expect("create");
    assert_eq!(returned, dir);
    assert_eq!(
        session.session_dir, dir,
        "create should set session.session_dir"
    );
    let json = dir.join("session.json");
    assert!(json.exists(), "session.json should exist at {json:?}");

    let loaded = store.get_by_path(&dir).expect("get_by_path");
    assert_eq!(loaded.id, sid);
    assert_eq!(loaded.container_name, format!("outrig-{}", sid.as_str()));
    assert_eq!(loaded.session_dir, dir);
    assert!(loaded.exit_code.is_none());
    assert!(loaded.ended_at.is_none());
}

#[test]
fn explicit_path_creates_session_and_symlink() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let canon_explicit = std::fs::canonicalize(explicit.path()).expect("canon");
    let mut session = sample_session(&sid);

    let returned = store
        .create(&sid, Some(explicit.path()), &mut session)
        .expect("create");
    assert_eq!(returned, canon_explicit);
    assert_eq!(session.session_dir, canon_explicit);

    let json = canon_explicit.join("session.json");
    assert!(
        json.exists(),
        "session.json should be inside the explicit dir"
    );

    let link = root.path().join(sid.as_str());
    let link_meta = std::fs::symlink_metadata(&link).expect("link metadata");
    assert!(
        link_meta.file_type().is_symlink(),
        "{link:?} must be a symlink"
    );
    let target = std::fs::read_link(&link).expect("read_link");
    assert_eq!(target, canon_explicit);
}

#[test]
fn explicit_path_with_existing_session_json_errors() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    std::fs::write(explicit.path().join("session.json"), b"{}").expect("preseed");

    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let mut session = sample_session(&sid);

    let err = store
        .create(&sid, Some(explicit.path()), &mut session)
        .expect_err("create should refuse");
    match err {
        OutrigError::Configuration(msg) => {
            assert!(
                msg.contains("already contains session.json"),
                "expected 'already contains' message, got: {msg}"
            );
        }
        other => panic!("expected Configuration error, got: {other:?}"),
    }
}

#[test]
fn list_includes_auto_and_symlinked_newest_first() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());

    // Older auto session.
    let sid_auto = SessionId("20260430T091203-44d2".into());
    let mut auto_session = sample_session(&sid_auto);
    store
        .create(&sid_auto, None, &mut auto_session)
        .expect("auto");

    // Newer symlinked session.
    let sid_sym = SessionId("20260501T141907-9b1c".into());
    let canon_explicit = std::fs::canonicalize(explicit.path()).expect("canon");
    let mut sym_session = sample_session(&sid_sym);
    store
        .create(&sid_sym, Some(explicit.path()), &mut sym_session)
        .expect("sym");

    let listed = store.list().expect("list");
    assert_eq!(listed.len(), 2, "should see both sessions");
    // Newest first: 0501 > 0430.
    assert_eq!(listed[0].id, sid_sym);
    assert_eq!(listed[1].id, sid_auto);
    assert_eq!(
        listed[0].link_target.as_deref(),
        Some(canon_explicit.as_path())
    );
    assert!(listed[1].link_target.is_none());
}

#[test]
fn get_by_id_and_get_by_path_round_trip() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let canon = std::fs::canonicalize(explicit.path()).expect("canon");
    let mut session = sample_session(&sid);
    store
        .create(&sid, Some(explicit.path()), &mut session)
        .expect("create");

    let (resolved_dir, by_id) = store.get_by_id(&sid).expect("get_by_id");
    assert_eq!(resolved_dir, canon);
    assert_eq!(by_id.link_target.as_deref(), Some(canon.as_path()));

    let by_path = store.get_by_path(&canon).expect("get_by_path");
    // Equality up to link_target (set only by id-based access).
    assert_eq!(by_id.id, by_path.id);
    assert_eq!(by_id.session_dir, by_path.session_dir);
    assert_eq!(by_id.container_name, by_path.container_name);
    assert!(by_path.link_target.is_none());
}

#[test]
fn finalize_writes_ended_at_and_exit_code() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let mut session = sample_session(&sid);
    store.create(&sid, None, &mut session).expect("create");

    let ended = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_137);
    store.finalize(&sid, ended, 42).expect("finalize");

    let (_, loaded) = store.get_by_id(&sid).expect("get_by_id");
    assert_eq!(loaded.ended_at, Some(ended));
    assert_eq!(loaded.exit_code, Some(42));
}

#[test]
fn remove_by_id_auto_removes_dir() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = root.path().join(sid.as_str());
    let mut session = sample_session(&sid);
    store.create(&sid, None, &mut session).expect("create");
    assert!(dir.exists());

    store.remove_by_id(&sid).expect("remove");
    assert!(!dir.exists(), "auto session dir should be gone");
}

#[test]
fn remove_by_id_symlinked_removes_target_and_link() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let canon = std::fs::canonicalize(explicit.path()).expect("canon");
    let mut session = sample_session(&sid);
    store
        .create(&sid, Some(explicit.path()), &mut session)
        .expect("create");

    let link = root.path().join(sid.as_str());
    assert!(link.exists());
    assert!(canon.join("session.json").exists());

    store.remove_by_id(&sid).expect("remove");
    assert!(
        !canon.join("session.json").exists(),
        "explicit dir contents should be removed"
    );
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "symlink at root should be removed"
    );
    // tempfile cleanup of `explicit` is fine even though the dir is now gone.
    drop(explicit);
}

#[test]
fn remove_by_path_cleans_dangling_symlink() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit_parent = tempfile::tempdir().expect("tempdir explicit_parent");
    let explicit = explicit_parent.path().join("run-x");
    std::fs::create_dir_all(&explicit).expect("mkdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let canon = std::fs::canonicalize(&explicit).expect("canon");
    let mut session = sample_session(&sid);
    store
        .create(&sid, Some(&explicit), &mut session)
        .expect("create");

    let link = root.path().join(sid.as_str());
    assert!(link.exists());

    store.remove_by_path(&canon).expect("remove_by_path");
    assert!(!canon.exists(), "explicit dir should be removed");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "dangling symlink should also be cleaned"
    );
}
