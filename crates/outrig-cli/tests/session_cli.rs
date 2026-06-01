//! Integration tests for `outrig_cli::cli::ls`, `outrig_cli::cli::logs`, and
//! `outrig_cli::cli::discard`. Each test owns its own tempdir as the session
//! root, builds sessions on disk via `SessionStore`, and drives the inner
//! `execute_with` form of each command through `tokio::io::duplex` halves.
//!
//! No podman dependency: the discard tests inject their own running-check
//! closure.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use clap::Parser;
use outrig_cli::cli::{clean, discard, logs, ls};
use outrig_cli::session::{Session, SessionId, SessionStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

mod common;
use common::sample_session;

async fn drain<R: AsyncReadExt + Unpin>(mut r: R) -> String {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

// -------- ls --------

#[tokio::test]
async fn ls_lists_newest_first() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());

    for sid_s in ["20260430T091203-44d2", "20260501T134412-3f2a"] {
        let sid = SessionId(sid_s.into());
        let mut s = sample_session(&sid);
        store.create(&sid, None, &mut s).expect("create");
    }

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    let rc = ls::execute_with(&mut sw, &mut ew, &store)
        .await
        .expect("ls");
    drop(sw);
    drop(ew);
    assert_eq!(rc, 0);

    let out = drain(stdout_r).await;
    let p_new = out.find("20260501T134412-3f2a").expect("newer present");
    let p_old = out.find("20260430T091203-44d2").expect("older present");
    assert!(p_new < p_old, "newer should come first; got\n{out}");
    assert!(out.contains("ID"));
    assert!(out.contains("STARTED"));
    assert!(out.contains("DURATION"));
    assert!(out.contains("IMAGE"));
    assert!(out.contains("EXIT"));
}

#[tokio::test]
async fn ls_marks_symlinked_with_target() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());

    let sid_auto = SessionId("20260430T091203-44d2".into());
    store
        .create(&sid_auto, None, &mut sample_session(&sid_auto))
        .expect("auto");

    let sid_link = SessionId("20260501T141907-9b1c".into());
    store
        .create(
            &sid_link,
            Some(explicit.path()),
            &mut sample_session(&sid_link),
        )
        .expect("link");

    let (mut sw, stdout_r) = duplex(8192);
    let (mut ew, _stderr_r) = duplex(4096);
    ls::execute_with(&mut sw, &mut ew, &store)
        .await
        .expect("ls");
    drop(sw);
    drop(ew);

    let out = drain(stdout_r).await;
    let canon = std::fs::canonicalize(explicit.path()).expect("canon");
    let needle = format!("-> {}", canon.display());
    assert!(out.contains(&needle), "expected {needle:?} in:\n{out}");
}

#[tokio::test]
async fn ls_running_session_renders_in_flight() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    // sample_session leaves ended_at + exit_code at None already.
    store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    ls::execute_with(&mut sw, &mut ew, &store)
        .await
        .expect("ls");
    drop(sw);
    drop(ew);

    let out = drain(stdout_r).await;
    let row = out
        .lines()
        .find(|l| l.contains(sid.as_str()))
        .expect("data row");
    // EXIT is the trailing field; running sessions render it as "-".
    assert!(
        row.trim_end().ends_with(" -") || row.trim_end().ends_with(" -  -> "),
        "expected trailing exit '-' in {row:?}"
    );
}

#[tokio::test]
async fn ls_empty_root_is_ok() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let (mut sw, _stdout_r) = duplex(4096);
    let (mut ew, stderr_r) = duplex(4096);
    ls::execute_with(&mut sw, &mut ew, &store)
        .await
        .expect("ls");
    drop(sw);
    drop(ew);

    let err = drain(stderr_r).await;
    assert!(err.contains("no sessions"), "got stderr:\n{err}");
}

// -------- logs --------

fn write_log(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).expect("logs dir");
    let p = logs.join(name);
    std::fs::write(&p, content).expect("write log");
    p
}

#[tokio::test]
async fn logs_no_server_lists_files_with_sizes() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let mut s = sample_session(&sid);
    let dir = store.create(&sid, None, &mut s).expect("create");
    write_log(&dir, "fs.stderr", b"hello fs\n");
    write_log(&dir, "shell.stderr", &vec![b'x'; 1228]);

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, stderr_r) = duplex(4096);
    let rc = logs::execute_with(&mut sw, &mut ew, &dir.join("logs"), None, false)
        .await
        .expect("logs");
    drop(sw);
    drop(ew);
    assert_eq!(rc, 0);

    let out = drain(stdout_r).await;
    let err = drain(stderr_r).await;
    assert!(err.contains("logs in"), "stderr should announce dir: {err}");
    assert!(out.contains("fs"), "stdout should mention 'fs': {out}");
    assert!(
        out.contains("shell"),
        "stdout should mention 'shell': {out}"
    );
    assert!(out.contains("KiB"), "stdout should size-format: {out}");
    assert!(!out.contains("fs.stderr"), "should strip .stderr: {out}");
}

#[tokio::test]
async fn logs_with_server_cats_file() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let mut s = sample_session(&sid);
    let dir = store.create(&sid, None, &mut s).expect("create");
    let body = b"line one\nline two\n";
    write_log(&dir, "fs.stderr", body);

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    logs::execute_with(&mut sw, &mut ew, &dir.join("logs"), Some("fs"), false)
        .await
        .expect("logs");
    drop(sw);
    drop(ew);

    let out = drain(stdout_r).await;
    assert_eq!(out.as_bytes(), body);
}

#[tokio::test]
async fn logs_substring_resolves_uniquely() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    for sid_s in ["20260430T091203-44d2", "20260501T134412-3f2a"] {
        let sid = SessionId(sid_s.into());
        let dir = store
            .create(&sid, None, &mut sample_session(&sid))
            .expect("create");
        write_log(&dir, "fs.stderr", format!("body {sid_s}\n").as_bytes());
    }

    let (dir, _) = outrig_cli::cli::resolve_session_arg(&store, "1344").expect("substring");
    let logs_dir = dir.join("logs");

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    logs::execute_with(&mut sw, &mut ew, &logs_dir, Some("fs"), false)
        .await
        .expect("logs");
    drop(sw);
    drop(ew);
    let out = drain(stdout_r).await;
    assert!(
        out.contains("20260501T134412-3f2a"),
        "expected unique match content: {out}"
    );
}

#[tokio::test]
async fn logs_substring_ambiguous_errors() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    for sid_s in ["20260501T134412-3f2a", "20260501T141907-9b1c"] {
        let sid = SessionId(sid_s.into());
        store
            .create(&sid, None, &mut sample_session(&sid))
            .expect("create");
    }

    let err = outrig_cli::cli::resolve_session_arg(&store, "20260501")
        .expect_err("ambiguous should error");
    let msg = format!("{err}");
    assert!(msg.contains("ambiguous"), "expected ambiguous error: {msg}");
    assert!(msg.contains("3f2a") && msg.contains("9b1c"), "{msg}");
}

#[tokio::test]
async fn logs_session_dir_skips_root_lookup() {
    // Session lives at an explicit path; no root needed.
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path();
    let body = b"direct cat\n";
    write_log(dir, "fs.stderr", body);

    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    logs::execute_with(&mut sw, &mut ew, &dir.join("logs"), Some("fs"), false)
        .await
        .expect("logs");
    drop(sw);
    drop(ew);
    let out = drain(stdout_r).await;
    assert_eq!(out.as_bytes(), body);
}

#[tokio::test]
async fn logs_follow_streams_appended_writes() {
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path();
    let path = write_log(dir, "fs.stderr", b"first\n");
    let logs_dir = dir.join("logs");

    let (mut sw, mut stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);

    let path_for_writer = path.clone();
    let writer = tokio::spawn(async move {
        // Let cat_file complete first dump, then start polling.
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path_for_writer)
            .await
            .expect("open append");
        f.write_all(b"second\n").await.expect("append");
        f.flush().await.expect("flush");
    });

    let follow = tokio::spawn(async move {
        // Cancel the follow loop after we've had time to observe the append.
        tokio::time::timeout(
            Duration::from_secs(2),
            logs::execute_with(&mut sw, &mut ew, &logs_dir, Some("fs"), true),
        )
        .await
    });

    // Read until we see both lines or timeout.
    let mut acc = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 256];
        loop {
            let n = stdout_r.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(b"second".len()).any(|w| w == b"second") {
                break;
            }
        }
    })
    .await;

    writer.await.expect("writer joined");
    let _ = follow.await;
    let s = String::from_utf8_lossy(&acc);
    assert!(s.contains("first"), "missing initial dump: {s}");
    assert!(s.contains("second"), "missing appended write: {s}");
}

#[tokio::test]
async fn logs_follow_handles_truncation() {
    let scratch = tempfile::tempdir().expect("scratch");
    let dir = scratch.path();
    let path = write_log(dir, "fs.stderr", b"original\n");
    let logs_dir = dir.join("logs");

    let (mut sw, mut stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);

    let path_for_writer = path.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Truncate first, give the polling loop a chance to observe len=0,
        // then rewrite. Tests the size-shrunk -> reopen branch in
        // `follow_file`. A single atomic write would happen too fast for
        // a 200ms poll to detect.
        let f = tokio::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&path_for_writer)
            .await
            .expect("open truncate");
        f.sync_all().await.expect("sync truncate");
        drop(f);
        tokio::time::sleep(Duration::from_millis(400)).await;
        tokio::fs::write(&path_for_writer, b"rotated-content\n")
            .await
            .expect("rewrite");
    });

    let follow = tokio::spawn(async move {
        tokio::time::timeout(
            Duration::from_secs(2),
            logs::execute_with(&mut sw, &mut ew, &logs_dir, Some("fs"), true),
        )
        .await
    });

    let mut acc = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let mut buf = [0u8; 256];
        loop {
            let n = stdout_r.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            acc.extend_from_slice(&buf[..n]);
            if acc.windows(b"rotated".len()).any(|w| w == b"rotated") {
                break;
            }
        }
    })
    .await;

    writer.await.expect("writer joined");
    let _ = follow.await;
    let s = String::from_utf8_lossy(&acc);
    assert!(s.contains("original"), "missing initial: {s}");
    assert!(s.contains("rotated-content"), "missing rotated: {s}");
}

// -------- discard --------

fn args_for_id(id: &str, yes: bool) -> discard::DiscardArgs {
    discard::DiscardArgs {
        session: Some(id.to_string()),
        yes,
        session_dir: None,
    }
}

fn args_for_dir(dir: &std::path::Path, yes: bool) -> discard::DiscardArgs {
    discard::DiscardArgs {
        session: None,
        yes,
        session_dir: Some(dir.to_path_buf()),
    }
}

#[tokio::test]
async fn discard_yes_removes_auto_session() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");
    assert!(dir.exists());

    let (mut ew, _stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = args_for_id(sid.as_str(), true);
    let rc = discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(false) })
        .await
        .expect("discard");
    drop(ew);
    assert_eq!(rc, 0);
    assert!(!dir.exists(), "auto dir should be removed: {dir:?}");
}

#[tokio::test]
async fn discard_yes_removes_symlinked_session() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let target = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create");
    let link = root.path().join(sid.as_str());
    assert!(target.exists());
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );

    let (mut ew, stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = args_for_id(sid.as_str(), true);
    discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(false) })
        .await
        .expect("discard");
    drop(ew);

    assert!(!target.exists(), "link target should be removed");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "symlink should be gone"
    );
    let msg = drain(stderr_r).await;
    assert!(msg.contains("(symlink)"), "expected symlink notice: {msg}");
}

#[tokio::test]
async fn discard_refuses_when_running() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    let (mut ew, _stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = args_for_id(sid.as_str(), true);
    let err = discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(true) })
        .await
        .expect_err("should refuse");
    drop(ew);
    let msg = format!("{err}");
    assert!(msg.contains("still running"), "{msg}");
    assert!(msg.contains("outrig-20260501T134412-3f2a"), "{msg}");
    assert!(dir.exists(), "dir must remain after refusal");
}

#[tokio::test]
async fn discard_session_dir_path_works() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T141907-9b1c".into());
    let target = store
        .create(&sid, Some(explicit.path()), &mut sample_session(&sid))
        .expect("create");
    assert!(target.exists());

    let (mut ew, _stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = args_for_dir(&target, true);
    discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(false) })
        .await
        .expect("discard");
    drop(ew);

    assert!(!target.exists(), "explicit dir should be removed");
    let link = root.path().join(sid.as_str());
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "remove_by_path should sweep the dangling symlink"
    );
}

#[tokio::test]
async fn discard_without_yes_aborts_on_n() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    let (mut ew, _stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(&b"n\n"[..]);
    let args = args_for_id(sid.as_str(), false);
    let rc = discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(false) })
        .await
        .expect("discard");
    drop(ew);
    assert_eq!(rc, 0);
    assert!(dir.exists(), "dir must remain after 'n' answer");
}

#[tokio::test]
async fn discard_accepts_yes_at_prompt() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut sample_session(&sid))
        .expect("create");

    let (mut ew, _stderr_r) = duplex(4096);
    let stdin = tokio::io::BufReader::new(&b"yes\n"[..]);
    let args = args_for_id(sid.as_str(), false);
    discard::execute_with(&mut ew, stdin, &store, &args, |_| async { Ok(false) })
        .await
        .expect("discard");
    drop(ew);
    assert!(!dir.exists());
}

// -------- clean --------

const DAY_SECS: u64 = 24 * 60 * 60;

fn days(count: u64) -> Duration {
    Duration::from_secs(count * DAY_SECS)
}

fn clean_args(older_than: Duration, yes: bool) -> clean::CleanArgs {
    clean::CleanArgs { older_than, yes }
}

fn clean_now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
}

fn session_with_age(
    id: &SessionId,
    now: SystemTime,
    started_age: Duration,
    ended_age: Option<Duration>,
) -> Session {
    let mut session = sample_session(id);
    session.started_at = now - started_age;
    session.ended_at = ended_age.map(|age| now - age);
    session.exit_code = session.ended_at.map(|_| 0);
    session
}

#[test]
fn clean_default_older_than_is_30_days() {
    let args = clean::CleanArgs::try_parse_from(["clean"]).expect("parse clean args");
    assert_eq!(args.older_than, clean::DEFAULT_OLDER_THAN);
    assert!(!args.yes);
}

#[test]
fn clean_duration_parser_accepts_units_and_rejects_invalid() {
    assert_eq!(clean::parse_duration("7d").expect("days"), days(7));
    assert_eq!(
        clean::parse_duration("12h").expect("hours"),
        Duration::from_secs(12 * 60 * 60)
    );
    assert_eq!(
        clean::parse_duration("45m").expect("minutes"),
        Duration::from_secs(45 * 60)
    );
    assert_eq!(
        clean::parse_duration("10s").expect("seconds"),
        Duration::from_secs(10)
    );

    assert!(clean::parse_duration("0d").is_err());
    assert!(clean::parse_duration("30x").is_err());
    assert!(clean::parse_duration("days").is_err());
}

#[tokio::test]
async fn clean_default_30d_removes_only_old_finished_sessions() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();

    let old_sid = SessionId("20260501T134412-3f2a".into());
    let old_dir = store
        .create(
            &old_sid,
            None,
            &mut session_with_age(&old_sid, now, days(40), Some(days(31))),
        )
        .expect("old create");
    let recent_sid = SessionId("20260502T134412-4a4a".into());
    let recent_dir = store
        .create(
            &recent_sid,
            None,
            &mut session_with_age(&recent_sid, now, days(29), Some(days(29))),
        )
        .expect("recent create");

    let (mut ew, stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = clean_args(clean::DEFAULT_OLDER_THAN, true);
    let rc = clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);
    assert_eq!(rc, 0);

    assert!(!old_dir.exists(), "old session should be removed");
    assert!(recent_dir.exists(), "recent session should remain");
    let err = drain(stderr_r).await;
    assert!(err.contains("older than 30d"), "stderr:\n{err}");
    assert!(err.contains("cleaned 1 session"), "stderr:\n{err}");
}

#[tokio::test]
async fn clean_custom_older_than_uses_requested_cutoff() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();

    let old_sid = SessionId("20260501T134412-3f2a".into());
    let old_dir = store
        .create(
            &old_sid,
            None,
            &mut session_with_age(
                &old_sid,
                now,
                Duration::from_secs(8_000),
                Some(Duration::from_secs(7_200)),
            ),
        )
        .expect("old create");
    let recent_sid = SessionId("20260502T134412-4a4a".into());
    let recent_dir = store
        .create(
            &recent_sid,
            None,
            &mut session_with_age(
                &recent_sid,
                now,
                Duration::from_secs(1_000),
                Some(Duration::from_secs(1_000)),
            ),
        )
        .expect("recent create");

    let (mut ew, stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = clean_args(Duration::from_secs(60 * 60), true);
    clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);

    assert!(!old_dir.exists(), "old session should be removed");
    assert!(recent_dir.exists(), "recent session should remain");
    let err = drain(stderr_r).await;
    assert!(err.contains("older than 1h"), "stderr:\n{err}");
}

#[tokio::test]
async fn clean_without_yes_aborts_on_n() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(
            &sid,
            None,
            &mut session_with_age(&sid, now, days(40), Some(days(31))),
        )
        .expect("create");

    let (mut ew, stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(&b"n\n"[..]);
    let args = clean_args(clean::DEFAULT_OLDER_THAN, false);
    let rc = clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);

    assert_eq!(rc, 0);
    assert!(dir.exists(), "dir must remain after abort");
    let err = drain(stderr_r).await;
    assert!(err.contains("will remove 1 session"), "stderr:\n{err}");
    assert!(err.contains("aborted"), "stderr:\n{err}");
}

#[tokio::test]
async fn clean_accepts_yes_at_prompt() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(
            &sid,
            None,
            &mut session_with_age(&sid, now, days(40), Some(days(31))),
        )
        .expect("create");

    let (mut ew, _stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(&b"yes\n"[..]);
    let args = clean_args(clean::DEFAULT_OLDER_THAN, false);
    clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);

    assert!(!dir.exists());
}

#[tokio::test]
async fn clean_skips_running_sessions() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut session_with_age(&sid, now, days(40), None))
        .expect("create");
    let running = format!("outrig-{}", sid.as_str());

    let (mut ew, stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = clean_args(clean::DEFAULT_OLDER_THAN, true);
    clean::execute_with(&mut ew, stdin, &store, &args, now, move |name| {
        let is_running = name == running.as_str();
        async move { Ok(is_running) }
    })
    .await
    .expect("clean");
    drop(ew);

    assert!(dir.exists(), "running session should remain");
    let err = drain(stderr_r).await;
    assert!(err.contains("skipped running sessions"), "stderr:\n{err}");
    assert!(
        err.contains("no stopped sessions older than 30d"),
        "stderr:\n{err}"
    );
}

#[tokio::test]
async fn clean_removes_stale_unfinalized_non_running_sessions() {
    let root = tempfile::tempdir().expect("tempdir root");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();
    let sid = SessionId("20260501T134412-3f2a".into());
    let dir = store
        .create(&sid, None, &mut session_with_age(&sid, now, days(40), None))
        .expect("create");

    let (mut ew, _stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = clean_args(clean::DEFAULT_OLDER_THAN, true);
    clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);

    assert!(!dir.exists(), "stale non-running session should be removed");
}

#[tokio::test]
async fn clean_removes_symlinked_session_target_and_link() {
    let root = tempfile::tempdir().expect("tempdir root");
    let explicit = tempfile::tempdir().expect("tempdir explicit");
    let store = SessionStore::new(root.path().to_path_buf());
    let now = clean_now();
    let sid = SessionId("20260501T134412-3f2a".into());
    let target = store
        .create(
            &sid,
            Some(explicit.path()),
            &mut session_with_age(&sid, now, days(40), Some(days(31))),
        )
        .expect("create");
    let link = root.path().join(sid.as_str());
    assert!(target.exists());
    assert!(
        std::fs::symlink_metadata(&link)
            .expect("link")
            .file_type()
            .is_symlink()
    );

    let (mut ew, stderr_r) = duplex(8192);
    let stdin = tokio::io::BufReader::new(tokio::io::empty());
    let args = clean_args(clean::DEFAULT_OLDER_THAN, true);
    clean::execute_with(&mut ew, stdin, &store, &args, now, |_| async { Ok(false) })
        .await
        .expect("clean");
    drop(ew);

    assert!(!target.exists(), "target should be removed");
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "symlink should be removed"
    );
    let err = drain(stderr_r).await;
    assert!(err.contains("(symlink)"), "stderr:\n{err}");
}

// -------- wiring sanity --------

#[tokio::test]
async fn session_root_flag_overrides_default() {
    // Two roots: A has one session, B has another. Constructing the store
    // with each root (as `ls::execute` would after resolution) yields the
    // expected listing. This validates the resolver downstream wiring without
    // spawning the binary.
    let root_a = tempfile::tempdir().expect("a");
    let root_b = tempfile::tempdir().expect("b");
    let sid_a = SessionId("20260101T000000-aaaa".into());
    let sid_b = SessionId("20260101T000000-bbbb".into());
    SessionStore::new(root_a.path().to_path_buf())
        .create(&sid_a, None, &mut sample_session(&sid_a))
        .expect("a");
    SessionStore::new(root_b.path().to_path_buf())
        .create(&sid_b, None, &mut sample_session(&sid_b))
        .expect("b");

    let store_a = SessionStore::new(root_a.path().to_path_buf());
    let (mut sw, stdout_r) = duplex(4096);
    let (mut ew, _stderr_r) = duplex(4096);
    ls::execute_with(&mut sw, &mut ew, &store_a)
        .await
        .expect("ls a");
    drop(sw);
    drop(ew);
    let out = drain(stdout_r).await;
    assert!(out.contains(sid_a.as_str()), "expected A: {out}");
    assert!(!out.contains(sid_b.as_str()), "should not see B: {out}");
}
