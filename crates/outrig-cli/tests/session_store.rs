//! SessionStore on-disk behavior. Each test owns its own tempdir as the
//! session root; symlinks land inside that tree so cleanup is automatic.

use std::sync::Barrier;
use std::time::{Duration, SystemTime};

use outrig::error::OutrigError;
use outrig_cli::error::CliError;
use outrig_cli::session::{self, SessionId, SessionStore};

mod common;
use common::{
    as_legacy_image_key, assert_only_user_files_left, drop_image_tag, sample_session,
    write_outrig_entries, write_raw_session, write_user_files,
};

#[test]
fn auto_path_creates_session_json() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = root.path().join(sid.as_str());
    let mut session = sample_session(&sid);

    let returned = store.create(&sid, None, &mut session).expect("create").0;
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
        .expect("create")
        .0;
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
        CliError::Outrig(OutrigError::Configuration(msg)) => {
            assert!(
                msg.contains("holds an earlier session's record"),
                "expected 'earlier session' message, got: {msg}"
            );
        }
        other => panic!("expected Configuration error, got: {other:?}"),
    }
    assert!(
        std::fs::symlink_metadata(root.path().join(sid.as_str())).is_err(),
        "a refused directory gets no link"
    );
}

/// #326: an explicit directory has to start empty. One that already held the
/// user's files used to be taken, and discard then removed it whole.
#[test]
fn explicit_path_that_is_not_empty_is_refused_before_anything_is_written() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    std::fs::write(explicit.path().join("KEEP_ME.txt"), b"my notes\n").expect("preseed");

    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let mut session = sample_session(&sid);

    let err = store
        .create(&sid, Some(explicit.path()), &mut session)
        .expect_err("create should refuse");
    match err {
        CliError::Outrig(OutrigError::Configuration(msg)) => {
            assert!(
                msg.contains("is not empty (it holds KEEP_ME.txt)"),
                "expected the entry named, got: {msg}"
            );
        }
        other => panic!("expected Configuration error, got: {other:?}"),
    }
    let names: Vec<_> = std::fs::read_dir(explicit.path())
        .expect("read explicit")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["KEEP_ME.txt"], "nothing is written beside the file");
    assert!(
        std::fs::symlink_metadata(root.path().join(sid.as_str())).is_err(),
        "a refused directory gets no link"
    );
}

#[test]
fn explicit_path_that_does_not_exist_is_created() {
    let root = tempfile::tempdir().expect("tempdir root");
    let parent = tempfile::tempdir().expect("tempdir parent");
    let explicit = parent.path().join("runs/run-1");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let mut session = sample_session(&sid);

    let returned = store
        .create(&sid, Some(&explicit), &mut session)
        .expect("create")
        .0;
    let canon = std::fs::canonicalize(&explicit).expect("the directory should be created");
    assert_eq!(returned, canon);
    assert!(canon.join("session.json").exists());
    let target = std::fs::read_link(root.path().join(sid.as_str())).expect("read_link");
    assert_eq!(target, canon);
}

#[test]
fn explicit_path_that_is_a_file_is_refused() {
    let root = tempfile::tempdir().expect("tempdir root");
    let parent = tempfile::tempdir().expect("tempdir parent");
    let file = parent.path().join("notes.txt");
    std::fs::write(&file, b"my notes\n").expect("write file");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let mut session = sample_session(&sid);

    let err = store
        .create(&sid, Some(&file), &mut session)
        .expect_err("create should refuse");
    match err {
        CliError::Outrig(OutrigError::Configuration(msg)) => {
            assert!(
                msg.contains("exists and is not a directory"),
                "expected 'not a directory' message, got: {msg}"
            );
        }
        other => panic!("expected Configuration error, got: {other:?}"),
    }
    assert_eq!(std::fs::read(&file).expect("file kept"), b"my notes\n");
}

/// #430: checking that an explicit directory was free, then writing into it,
/// left a gap two simultaneous launches both fit through. The write is the
/// claim now, so exactly one wins and the other is refused.
#[test]
fn concurrent_creates_on_one_explicit_dir_claim_it_once() {
    let a = SessionId("20261005T205313-aaaa".into());
    let b = SessionId("20261005T205313-bbbb".into());
    for _ in 0..10 {
        let root = tempfile::tempdir().expect("tempdir root");
        let explicit = tempfile::tempdir().expect("tempdir explicit");
        let store = SessionStore::new(root.path().to_path_buf());
        let barrier = Barrier::new(2);
        let launch = |sid: &SessionId| {
            let mut session = sample_session(sid);
            barrier.wait();
            store.create(sid, Some(explicit.path()), &mut session)
        };
        let (ra, rb) = std::thread::scope(|scope| {
            let (ha, hb) = (scope.spawn(|| launch(&a)), scope.spawn(|| launch(&b)));
            (
                ha.join().expect("create thread"),
                hb.join().expect("create thread"),
            )
        });
        let (winner, loser) = match (ra, rb) {
            (Ok(_), Err(e)) => (&a, e),
            (Err(e), Ok(_)) => (&b, e),
            both => panic!("exactly one create should win, got {both:?}"),
        };

        assert!(
            matches!(loser, CliError::Outrig(OutrigError::Configuration(_))),
            "the loser is refused, got: {loser:?}"
        );
        let ids: Vec<_> = store
            .list()
            .expect("list")
            .sessions
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            ids,
            std::slice::from_ref(winner),
            "one link, reading the winner's record"
        );
        assert_eq!(
            std::fs::read_dir(explicit.path())
                .expect("read explicit")
                .count(),
            1,
            "the loser leaves no temp file behind"
        );
    }
}

/// #382: `create` made an auto-allocated directory with `create_dir_all`, which
/// succeeds on one that exists, so a second launch given the same id wrote its
/// record over the first's. Making the directory is the claim now: the second is
/// refused and the first's record is as it was.
#[test]
fn a_second_create_for_one_auto_id_is_refused_and_the_first_record_kept() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261026T101500-3f2a1c9e".into());
    let mut first = sample_session(&sid);
    first.image_tag = "first".into();
    let (dir, _lock) = store.create(&sid, None, &mut first).expect("first create");

    let mut second = sample_session(&sid);
    second.image_tag = "second".into();
    let err = store
        .create(&sid, None, &mut second)
        .expect_err("the id is taken");

    assert!(
        matches!(&err, CliError::SessionIdTaken(taken) if *taken == dir),
        "expected SessionIdTaken for {}, got: {err:?}",
        dir.display()
    );
    assert_eq!(
        store.get_by_path(&dir).expect("read").image_tag,
        "first",
        "the first launch's record is not replaced"
    );
    assert!(
        session::owner_is_live(&dir),
        "the first launch still holds its claim"
    );
    assert_eq!(
        std::fs::read_dir(&dir).expect("read dir").count(),
        1,
        "the refused launch leaves no temp file behind"
    );
}

/// The same hole through a link: `create_dir_all` follows a `--session-dir` link
/// to the directory behind it and calls that the directory it made, so the
/// record went into the user's directory over the one already there.
#[test]
fn an_auto_id_held_by_an_explicit_link_is_refused() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261026T101501-3f2a1c9e".into());
    let mut first = sample_session(&sid);
    first.image_tag = "first".into();
    let (dir, _lock) = store
        .create(&sid, Some(explicit.path()), &mut first)
        .expect("first create");

    let mut second = sample_session(&sid);
    second.image_tag = "second".into();
    let err = store
        .create(&sid, None, &mut second)
        .expect_err("the link holds the id");

    let link = root.path().join(sid.as_str());
    assert!(
        matches!(&err, CliError::SessionIdTaken(taken) if *taken == link),
        "expected SessionIdTaken for {}, got: {err:?}",
        link.display()
    );
    assert_eq!(
        store.get_by_path(&dir).expect("read").image_tag,
        "first",
        "the record in the explicit directory is not replaced"
    );
    assert!(
        std::fs::symlink_metadata(root.path().join(sid.as_str()))
            .expect("stat link")
            .file_type()
            .is_symlink(),
        "the link is still a link"
    );
}

/// Two launches drawing one id at the same moment, which is what #382 is about,
/// and not one after the other: the directory is the claim, so exactly one wins
/// and the rest are refused with its record intact.
#[test]
fn concurrent_creates_of_one_auto_id_claim_it_once() {
    const LAUNCHES: usize = 4;
    let sid = SessionId("20261026T101502-3f2a1c9e".into());
    for _ in 0..10 {
        let root = tempfile::tempdir().expect("tempdir root");
        let store = SessionStore::new(root.path().to_path_buf());
        let barrier = Barrier::new(LAUNCHES);
        let results: Vec<_> = std::thread::scope(|scope| {
            let launches: Vec<_> = (0..LAUNCHES)
                .map(|n| {
                    let (store, barrier, sid) = (&store, &barrier, &sid);
                    scope.spawn(move || {
                        let mut session = sample_session(sid);
                        session.image_tag = format!("launch-{n}");
                        barrier.wait();
                        store
                            .create(sid, None, &mut session)
                            .map(|(_dir, lock)| (n, lock))
                    })
                })
                .collect();
            launches
                .into_iter()
                .map(|launch| launch.join().expect("create thread"))
                .collect()
        });

        let (won, lost): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
        assert_eq!(won.len(), 1, "exactly one launch claims the id");
        for loser in lost {
            let err = loser.expect_err("partitioned as lost");
            assert!(
                matches!(err, CliError::SessionIdTaken(_)),
                "a loser is refused for the id, got: {err:?}"
            );
        }
        let (winner, _lock) = won.into_iter().next().expect("one winner").expect("won");
        let (dir, record) = store.get_by_id(&sid).expect("the claimed session");
        assert_eq!(
            record.image_tag,
            format!("launch-{winner}"),
            "the record is the winner's"
        );
        assert_eq!(
            std::fs::read_dir(&dir).expect("read dir").count(),
            1,
            "the losers leave no temp file behind"
        );
    }
}

/// #382: a launch whose draw another launch holds draws again, and the record
/// it keeps is the one built for the id that stuck, not for the draws before.
#[test]
fn create_fresh_draws_again_when_the_id_is_taken() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let taken = SessionId("20261026T101503-aaaaaaaa".into());
    let fresh = SessionId("20261026T101503-bbbbbbbb".into());
    let mut holder = sample_session(&taken);
    holder.image_tag = "holder".into();
    let (taken_dir, _holder_lock) = store.create(&taken, None, &mut holder).expect("holder");

    let mut script = vec![taken.clone(), taken.clone(), fresh.clone()].into_iter();
    let mut built = Vec::new();
    let (session, dir, _lock) = store
        .create_fresh(
            || script.next().expect("drew past the script"),
            None,
            |sid| {
                built.push(sid.clone());
                sample_session(sid)
            },
        )
        .expect("create_fresh");

    assert_eq!(
        built,
        [taken.clone(), taken.clone(), fresh.clone()],
        "a record is built for every draw"
    );
    assert_eq!(session.id, fresh);
    assert_eq!(session.container_name, format!("outrig-{fresh}"));
    assert_eq!(dir, root.path().join(fresh.as_str()));
    assert_eq!(session.session_dir, dir);
    assert_eq!(
        store.get_by_path(&dir).expect("read").id,
        fresh,
        "the record on disk is the kept session's"
    );
    assert_eq!(
        store.get_by_path(&taken_dir).expect("read").image_tag,
        "holder",
        "the id that was taken is still its holder's"
    );
}

/// A root that answers every draw with "exists" is not bad luck, so the loop
/// ends instead of drawing for ever, with the last refusal as its error.
#[test]
fn create_fresh_gives_up_when_every_draw_is_taken() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let taken = SessionId("20261026T101504-aaaaaaaa".into());
    let (taken_dir, _holder_lock) = store
        .create(&taken, None, &mut sample_session(&taken))
        .expect("holder");

    let mut draws = 0;
    let err = store
        .create_fresh(
            || {
                draws += 1;
                taken.clone()
            },
            None,
            sample_session,
        )
        .expect_err("every id is taken");

    assert_eq!(draws, session::SESSION_ID_DRAWS);
    assert!(
        matches!(&err, CliError::SessionIdTaken(path) if *path == taken_dir),
        "the last refusal is the error, got: {err:?}"
    );
}

/// `setup` goes through `create_fresh` for `--session-dir` launches too, so it
/// has to carry an explicit directory through to `create` unchanged.
#[test]
fn create_fresh_makes_an_explicit_directory_session_too() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261026T101505-cccccccc".into());
    let canon = std::fs::canonicalize(explicit.path()).expect("canon");

    let (session, dir, _lock) = store
        .create_fresh(|| sid.clone(), Some(explicit.path()), sample_session)
        .expect("create_fresh");

    assert_eq!(dir, canon);
    assert_eq!(session.session_dir, canon);
    let (linked, record) = store.get_by_id(&sid).expect("the link resolves");
    assert_eq!(linked, canon);
    assert_eq!(record.id, sid);
}

/// What `new` draws: eight hex digits after the timestamp, and a name
/// `started_at` reads back, which is how a `clean` that cannot parse the record
/// still ages the directory.
#[test]
fn a_drawn_id_has_eight_random_digits() {
    let sid = SessionId::new();
    let (ts, rand) = sid.as_str().split_once('-').expect("a dash");

    assert_eq!(ts.len(), "yyyymmddTHHMMSS".len());
    assert_eq!(rand.len(), 8);
    assert!(rand.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(
        SessionId::started_at(sid.as_str()).is_some(),
        "{sid} reads back"
    );
}

/// Four digits is what outrig 0.2.2 and earlier wrote, so those sessions are on
/// disk and keep reading. Any other width is not an id.
#[test]
fn started_at_reads_both_widths_ids_have_had() {
    let four = SessionId::started_at("20260501T134412-3f2a").expect("four digits");
    let eight = SessionId::started_at("20260501T134412-3f2a1c9e").expect("eight digits");
    assert_eq!(four, eight, "the timestamp reads the same either way");

    for name in [
        "20260501T134412-3f2",
        "20260501T134412-3f2a1",
        "20260501T134412-3f2a1c",
        "20260501T134412-3f2a1c9",
        "20260501T134412-3f2a1c9e0",
        "20260501T134412-3f2g",
        "20260501T134412-",
        "20260501T134412",
        "notes",
    ] {
        assert!(
            SessionId::started_at(name).is_none(),
            "{name} is not a session id"
        );
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
    write_outrig_entries(&canon);

    let left = store.remove_by_id(&sid).expect("remove");
    assert!(
        left.is_empty(),
        "nothing but the record was there: {left:?}"
    );
    assert!(
        !canon.exists(),
        "a directory holding only the record should be removed"
    );
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "symlink at root should be removed"
    );
    // tempfile cleanup of `explicit` is fine even though the dir is now gone.
    drop(explicit);
}

/// #326: removal deletes `session.json`, `logs/`, and `outrig-enter` and
/// nothing else, so a directory that also holds the user's files stays with
/// them. Files are added after `create` because it now refuses a non-empty
/// directory; the result is the shape a record written before 0.2.2 has.
#[test]
fn remove_by_id_symlinked_keeps_what_outrig_did_not_write() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let canon = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create")
        .0;
    write_outrig_entries(&canon);
    write_user_files(&canon);

    let left = store.remove_by_id(&sid).expect("remove");
    assert_eq!(left, ["KEEP_ME.txt", "photos"]);
    assert_only_user_files_left(&canon);
    assert!(
        std::fs::symlink_metadata(root.path().join(sid.as_str())).is_err(),
        "the link goes even though the directory stays"
    );
}

#[test]
fn remove_by_id_auto_keeps_what_outrig_did_not_write() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create")
        .0;
    write_outrig_entries(&dir);
    write_user_files(&dir);

    let left = store.remove_by_id(&sid).expect("remove");
    assert_eq!(left, ["KEEP_ME.txt", "photos"]);
    assert_only_user_files_left(&dir);
}

#[test]
fn remove_by_path_keeps_what_outrig_did_not_write() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let canon = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create")
        .0;
    write_outrig_entries(&canon);
    write_user_files(&canon);

    let left = store.remove_by_path(&canon).expect("remove_by_path");
    assert_eq!(left, ["KEEP_ME.txt", "photos"]);
    assert_only_user_files_left(&canon);
    assert!(
        std::fs::symlink_metadata(root.path().join(sid.as_str())).is_err(),
        "the sweep removes the link to it"
    );
}

/// Before 0.2.2 a `--session-dir` could already hold a `logs` that was not a
/// directory: the run then failed to create its log directory and finalized
/// the record beside it. Removal takes the record and leaves that `logs`
/// rather than failing on it -- which would also stop `clean` on every run.
#[test]
fn remove_by_id_keeps_a_logs_that_is_not_a_directory() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let canon = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create")
        .0;
    std::fs::write(canon.join("logs"), b"my log\n").expect("write logs file");

    let left = store.remove_by_id(&sid).expect("remove");
    assert_eq!(left, ["logs"]);
    assert!(!canon.join("session.json").exists(), "the record goes");
    assert_eq!(
        std::fs::read(canon.join("logs")).expect("kept"),
        b"my log\n"
    );
}

/// The same for an `outrig-enter` that is a directory, and a `logs` that is a
/// symlink: outrig makes neither, so neither is outrig's to remove.
#[test]
fn remove_by_id_keeps_record_names_of_another_type() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let elsewhere = tempfile::tempdir().expect("tempdir elsewhere");
    std::fs::write(elsewhere.path().join("mine.txt"), b"mine\n").expect("write");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T120000-aaaa".into());
    let canon = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create")
        .0;
    std::fs::create_dir_all(canon.join("outrig-enter/inside")).expect("mkdir");
    std::os::unix::fs::symlink(elsewhere.path(), canon.join("logs")).expect("symlink");

    let left = store.remove_by_id(&sid).expect("remove");
    assert_eq!(left, ["logs", "outrig-enter"]);
    assert!(!canon.join("session.json").exists(), "the record goes");
    assert!(canon.join("outrig-enter/inside").is_dir());
    assert!(
        std::fs::symlink_metadata(canon.join("logs"))
            .expect("link kept")
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read(elsewhere.path().join("mine.txt")).expect("target kept"),
        b"mine\n"
    );
}

/// #337: given the `<root>/<sid>` link rather than the directory, the record is
/// still the one in the directory it names. Removing the argument itself only
/// unlinked the link and left the record where nothing could find it.
#[test]
fn remove_by_path_through_the_link_removes_the_record_it_names() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20261003T110000-dddd".into());
    let canon = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create")
        .0;
    write_outrig_entries(&canon);
    let link = root.path().join(sid.as_str());

    let left = store.remove_by_path(&link).expect("remove_by_path");
    assert!(
        left.is_empty(),
        "nothing but the record was there: {left:?}"
    );
    assert!(!canon.exists(), "the record's directory should be removed");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "and the link to it"
    );
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
fn list_reports_a_link_whose_directory_is_gone() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit_parent = tempfile::tempdir().expect("tempdir explicit_parent");
    let explicit = explicit_parent.path().join("run-x");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    store
        .create(&sid, Some(&explicit), &mut sample_session(&sid))
        .expect("create");
    // Foreign content: a plain directory, and a link to a directory that
    // exists but holds no record. Neither is a session's leftover.
    std::fs::create_dir(root.path().join("plain")).expect("plain");
    let other = explicit_parent.path().join("other");
    std::fs::create_dir(&other).expect("other");
    std::os::unix::fs::symlink(&other, root.path().join("to-other")).expect("symlink");

    assert!(store.list().expect("list").dangling.is_empty());

    std::fs::remove_dir_all(&explicit).expect("rm");
    let listing = store.list().expect("list");
    assert!(listing.sessions.is_empty() && listing.skipped.is_empty());
    assert_eq!(listing.dangling.len(), 1);
    assert_eq!(listing.dangling[0].entry, sid.as_str());
    assert_eq!(
        listing.dangling[0].container_name(),
        format!("outrig-{sid}")
    );
}

#[test]
fn a_valid_relative_link_is_not_dangling() {
    let root = tempfile::tempdir().expect("tempdir root");
    std::fs::create_dir(root.path().join("foreign-dir")).expect("foreign");
    std::os::unix::fs::symlink("foreign-dir", root.path().join("to-other")).expect("symlink");
    std::os::unix::fs::symlink("gone-dir", root.path().join("to-gone")).expect("symlink");
    let store = SessionStore::new(root.path().to_path_buf());

    let listing = store.list().expect("list");
    let names: Vec<_> = listing.dangling.iter().map(|d| d.entry.as_str()).collect();
    assert_eq!(names, ["to-gone"]);
}

#[test]
fn unlink_dangling_leaves_a_directory_recreated_since_selection() {
    let root = tempfile::tempdir().expect("tempdir root");
    let parent = tempfile::tempdir().expect("tempdir parent");
    let explicit = parent.path().join("run-x");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    store
        .create(&sid, Some(&explicit), &mut sample_session(&sid))
        .expect("create");
    std::fs::remove_dir_all(&explicit).expect("rm");
    let link = store.list().expect("list").dangling.remove(0);

    // A new session claims the same path before the removal runs.
    std::fs::create_dir(&explicit).expect("recreate");
    std::fs::write(explicit.join("session.json"), "{}").expect("record");

    assert!(!store.unlink_dangling(&link).expect("unlink"));
    assert!(explicit.join("session.json").exists());
    assert!(std::fs::symlink_metadata(root.path().join(sid.as_str())).is_ok());
}

#[test]
fn remove_by_id_follows_a_relative_link_from_the_root_not_the_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("sessions");
    let sid = SessionId("20260501T141907-9b1c".into());
    let store = SessionStore::new(root.clone());
    // A decoy where the raw target lands if it is read against the cwd.
    let decoy = tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("decoy");
    let name = decoy.path().file_name().expect("name").to_owned();
    let decoy_run = decoy.path().join("run-x");
    std::fs::create_dir_all(decoy_run.join("logs")).expect("decoy logs");

    let real = root.join(&name).join("run-x");
    store
        .create(&sid, Some(&real), &mut sample_session(&sid))
        .expect("create");
    // Replace the absolute link with the relative spelling of the same one.
    let link = root.join(sid.as_str());
    std::fs::remove_file(&link).expect("unlink");
    std::os::unix::fs::symlink(std::path::Path::new(&name).join("run-x"), &link)
        .expect("relative link");

    store.remove_by_id(&sid).expect("remove");
    assert!(
        decoy_run.join("logs").exists(),
        "the cwd decoy is untouched"
    );
    assert!(!real.join("session.json").exists(), "the real record went");
    assert!(std::fs::symlink_metadata(&link).is_err());
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
        listed.skipped[0].reason().contains("image_tag"),
        "the reason should name the offending field, got: {}",
        listed.skipped[0].reason()
    );
    assert!(
        matches!(listed.skipped[0].error, CliError::SessionRecord { .. }),
        "a stale record has its own error, got: {:?}",
        listed.skipped[0].error
    );
    assert!(
        !listed.skipped[0]
            .error
            .to_string()
            .starts_with("configuration:"),
        "a stale record isn't user misconfiguration; got: {}",
        listed.skipped[0].error
    );
    assert_eq!(listed.skipped[0].dir, root.path().join(bad.as_str()));
    assert!(!listed.skipped[0].linked);
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

// -------- #366: the claim that says a session is live --------

#[test]
/// The window the issue is about: a record exists, `ended_at` is `None`, and
/// no container has been created yet because the image is still building.
/// The claim `create` hands back is the only thing that can say the session
/// is live, and it says so for as long as it is held.
fn create_claims_the_directory_until_the_lock_is_dropped() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let (dir, lock) = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    assert!(lock.is_some(), "a tempdir supports the claim");
    assert!(
        session::owner_is_live(&dir),
        "the launch still holds the session"
    );

    drop(lock);
    assert!(
        !session::owner_is_live(&dir),
        "the claim ends with the launch, as teardown's drop does"
    );
}

#[test]
/// The same for a `--session-dir` session: the claim is on the directory the
/// record is in, which is the link's target, not the link.
fn an_explicit_directory_is_claimed_too() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let (dir, lock) = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create");

    assert!(session::owner_is_live(&dir), "the target is claimed");
    drop(lock);
    assert!(!session::owner_is_live(&dir));
}

#[test]
/// Every update to the record renames a new file over `session.json`, so a
/// claim on *that* would be left behind on the replaced inode. The claim is
/// on the directory, which survives the rename.
fn the_claim_survives_updates_to_the_record() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let (dir, _lock) = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    store
        .set_sidecar_containers(&sid, &["outrig-20260501T134412-3f2a-tools".to_string()])
        .expect("set sidecars");
    assert!(
        session::owner_is_live(&dir),
        "a record rewritten under the claim is still claimed"
    );

    store
        .finalize(&sid, SystemTime::now(), 0)
        .expect("finalize");
    assert!(
        session::owner_is_live(&dir),
        "finalize does not release the claim; teardown drops it afterwards"
    );
}

#[test]
/// A record with no live owner -- what a session that ended, or one whose
/// process was killed outright, leaves behind. The kernel drops the claim
/// with the process, so these stay removable.
fn a_record_without_a_live_owner_is_not_claimed() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create")
        .0;

    assert!(
        !session::owner_is_live(&dir),
        "nothing holds the claim once create's caller drops it"
    );
    assert!(
        !session::owner_is_live(&root.path().join("20260501T134412-0000")),
        "a directory that does not exist has no live owner"
    );
}

#[test]
/// Two launches at once each claim their own directory, and neither reads as
/// the owner of the other's.
fn claims_are_per_directory() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let a = SessionId("20260501T134412-3f2a".into());
    let b = SessionId("20260501T134413-44d2".into());
    let (dir_a, lock_a) = store
        .create(&a, None, &mut sample_session(&a))
        .expect("create a");
    let (dir_b, _lock_b) = store
        .create(&b, None, &mut sample_session(&b))
        .expect("create b");

    assert!(session::owner_is_live(&dir_a));
    assert!(session::owner_is_live(&dir_b));
    drop(lock_a);
    assert!(!session::owner_is_live(&dir_a));
    assert!(
        session::owner_is_live(&dir_b),
        "one launch ending does not release another's claim"
    );
}
