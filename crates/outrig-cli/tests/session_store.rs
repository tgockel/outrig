//! SessionStore on-disk behavior. Each test owns its own tempdir as the
//! session root; symlinks land inside that tree so cleanup is automatic.

use std::time::{Duration, SystemTime};

use outrig::error::OutrigError;
use outrig_cli::session::{SessionId, SessionStore};

mod common;
use common::{as_legacy_image_key, drop_image_tag, sample_session, write_raw_session};

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

    let listed = store.list().expect("list").sessions;
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

/// Foreign directories under the session root must not break listings. This
/// is load-bearing for the error-context work: `read_session_json` reports a
/// missing file as `OutrigError::Path`, and `list` skips on exactly that
/// variant -- if the two ever drift apart, `outrig ls` starts failing outright
/// instead of ignoring the stray directory.
#[test]
fn list_skips_directories_without_session_json() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());

    let sid = SessionId("20260501T141907-9b1c".into());
    let mut session = sample_session(&sid);
    store.create(&sid, None, &mut session).expect("create");

    std::fs::create_dir_all(root.path().join("not-a-session")).expect("foreign dir");

    let listed = store.list().expect("list must not fail on foreign entries");
    assert!(
        listed.skipped.is_empty(),
        "a foreign dir isn't a broken record"
    );
    let listed = listed.sessions;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, sid);
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

#[test]
fn loads_legacy_agent_name_string() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = write_raw_session(root.path(), &sid, |v| {
        v["agent_name"] = serde_json::json!("coding");
    });

    let loaded = store.get_by_path(&dir).expect("get_by_path");
    assert_eq!(loaded.agent_name.as_deref(), Some("coding"));
}

#[test]
fn loads_session_with_agent_name_absent() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = write_raw_session(root.path(), &sid, |v| {
        v.as_object_mut().expect("object").remove("agent_name");
    });

    let loaded = store.get_by_path(&dir).expect("get_by_path");
    assert!(loaded.agent_name.is_none());
}

/// Records written before the 2026-06-01 `container` -> `image` rename stored
/// the image-config name under `container_config_name`. The value is the same
/// (the rename was pure), so the alias has to recover it -- a real session root
/// is full of these and they predate any chance to migrate.
#[test]
fn loads_legacy_container_config_name() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = write_raw_session(root.path(), &sid, as_legacy_image_key);

    let loaded = store.get_by_path(&dir).expect("get_by_path");
    assert_eq!(loaded.image_config_name.as_deref(), Some("coding"));
}

#[test]
fn loads_session_with_image_config_name_absent() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = write_raw_session(root.path(), &sid, |v| {
        v.as_object_mut()
            .expect("object")
            .remove("image_config_name");
    });

    let loaded = store.get_by_path(&dir).expect("get_by_path");
    assert!(loaded.image_config_name.is_none());
}

/// Any read-modify-write lazily migrates a legacy record onto the current key,
/// so the alias is a read-side shim only -- it never has to round-trip.
#[test]
fn finalize_upgrades_legacy_container_config_name() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = write_raw_session(root.path(), &sid, as_legacy_image_key);

    store
        .finalize(
            &sid,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
            0,
        )
        .expect("finalize");

    let raw = std::fs::read_to_string(dir.join("session.json")).expect("read");
    let value: serde_json::Value = serde_json::from_str(&raw).expect("parse");
    assert_eq!(value["image_config_name"], serde_json::json!("coding"));
    assert!(
        value.get("container_config_name").is_none(),
        "the legacy key should not survive a rewrite"
    );
}

/// One unreadable record must not cost the user every other session -- that
/// regression is exactly what made `outrig ls` fail outright on real data.
#[test]
fn list_skips_unparseable_session_json() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());

    let good = SessionId("20260501T141907-9b1c".into());
    let mut session = sample_session(&good);
    store.create(&good, None, &mut session).expect("create");

    // Missing a field that is still required, standing in for the next rename.
    let bad = SessionId("20260430T101500-1a2b".into());
    write_raw_session(root.path(), &bad, drop_image_tag);

    let listed = store.list().expect("list must survive a bad record");
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].id, good);
    assert_eq!(listed.skipped.len(), 1);
    assert_eq!(listed.skipped[0].entry, bad.as_str());
    assert!(
        listed.skipped[0].reason.contains("image_tag"),
        "the reason should name the offending field, got: {}",
        listed.skipped[0].reason
    );
    assert!(
        !listed.skipped[0].reason.starts_with("configuration:"),
        "a stale record isn't user misconfiguration; got: {}",
        listed.skipped[0].reason
    );
}

/// Naming a specific broken session must say so rather than report it missing.
#[test]
fn get_by_id_still_fails_on_unparseable_session_json() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260430T101500-1a2b".into());
    write_raw_session(root.path(), &sid, drop_image_tag);

    let err = store
        .get_by_id(&sid)
        .expect_err("must not silently succeed");
    assert!(err.to_string().contains("image_tag"), "got: {err}");
}

/// Frozen records, checked in byte-for-byte rather than round-tripped through
/// the current `Session`. Every other compat test here builds its fixture from
/// `sample_session`, so the baseline silently follows the struct: rename a
/// field and those tests keep passing while real on-disk records break, which
/// is exactly how the `container_config_name` rename shipped. These two don't
/// move, so the next unaliased rename fails here first.
#[test]
fn frozen_on_disk_records_still_load() {
    for (name, bytes) in [
        (
            "legacy-container-config-name",
            include_str!("fixtures/sessions/legacy-container-config-name.json"),
        ),
        (
            "current-schema",
            include_str!("fixtures/sessions/current-schema.json"),
        ),
    ] {
        let root = tempfile::tempdir().expect("tempdir root");
        let store = SessionStore::new(root.path().to_path_buf());
        let value: serde_json::Value = serde_json::from_str(bytes).expect("fixture is valid JSON");
        let sid = SessionId(value["id"].as_str().expect("id").to_string());
        let dir = root.path().join(sid.as_str());
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("session.json"), bytes).expect("write");

        let loaded = store
            .get_by_path(&dir)
            .unwrap_or_else(|e| panic!("fixture {name} must still load: {e}"));
        assert_eq!(loaded.id, sid, "fixture {name}");
        assert_eq!(
            loaded.image_config_name.as_deref(),
            Some("outrig-standard"),
            "fixture {name} should expose its image config under the current name"
        );
        // And the whole root lists without a skip.
        let listed = store.list().expect("list");
        assert_eq!(listed.sessions.len(), 1, "fixture {name}");
        assert!(
            listed.skipped.is_empty(),
            "fixture {name} must not be skipped"
        );
    }
}
