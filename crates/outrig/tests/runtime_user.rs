//! End-to-end smoke for `Container::bootstrap_user` and `exec_stdio`. Gated
//! behind `--features e2e` because it shells out to a real `podman` and
//! starts an `alpine:latest` container.
//!
//! Run with:
//!
//! ```sh
//! cargo test --features e2e runtime_user -- --nocapture
//! ```
//!
//! `alpine:latest` ships no `useradd`/`groupadd`, which is the point: the
//! bootstrap writes `/etc/passwd` and `/etc/group` from the host and needs
//! neither. The tests that still install the `shadow` package do so because
//! they plant a conflicting entry with `useradd` before bootstrapping, not
//! because bootstrap needs it.

#![cfg(feature = "e2e")]

mod common;

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use outrig::container::Container;
use outrig::{ExecOptions, Transcript};

use common::{
    entry_for_id, field_for_id, init_tracing, install_shadow, pull_alpine, read_stdout, root_cmd,
    root_stdout, run_capture, start_alpine,
};

#[tokio::test]
async fn bootstrap_then_id_matches_host() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;

    container.bootstrap_user().await.expect("bootstrap_user");
    assert!(container.user_name().is_some());
    assert!(container.group_name().is_some());

    let mut child = container
        .exec_stdio(&["id".to_string()], &ExecOptions::new())
        .await
        .expect("exec_stdio id");
    let out = read_stdout(&mut child).await;

    let expect_uid = format!("uid={}", container.uid());
    let expect_gid = format!("gid={}", container.gid());
    assert!(
        out.contains(&expect_uid),
        "expected `{expect_uid}` in `id` output, got: {out}"
    );
    assert!(
        out.contains(&expect_gid),
        "expected `{expect_gid}` in `id` output, got: {out}"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn workspace_writes_have_host_ownership() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    container.bootstrap_user().await.expect("bootstrap_user");

    let mut child = container
        .exec_stdio(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo hello > /workspace/test.txt".to_string(),
            ],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_stdio sh");
    let status = child.wait().await.expect("wait child");
    assert!(status.success(), "writer exited non-zero: {status:?}");

    let host_file = host_ws.path().join("test.txt");
    let meta = fs::metadata(&host_file).expect("stat host file");
    assert_eq!(
        meta.uid(),
        container.uid(),
        "file should be owned by host UID"
    );
    assert_eq!(
        meta.gid(),
        container.gid(),
        "file should be owned by host GID"
    );
    assert_eq!(
        fs::read_to_string(&host_file).expect("read"),
        "hello\n",
        "file contents should round-trip via the bind-mount"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

#[tokio::test]
async fn bootstrap_reuses_existing_entry() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    install_shadow(container.name());

    // Bootstrap should reuse whatever entry is already at the host UID/GID,
    // whether that's an auto-injection from `--userns=keep-id` or one we
    // manually plant here. Probe first; plant only if absent.
    let groups = root_stdout(container.name(), &["cat", "/etc/group"]);
    let expected_grp = match entry_for_id(&groups, container.gid()) {
        Some(existing) => existing,
        None => {
            let planted = "preexisting_grp";
            run_capture(root_cmd(container.name()).args([
                "groupadd",
                "--gid",
                &container.gid().to_string(),
                planted,
            ]));
            planted.to_string()
        }
    };
    let passwd = root_stdout(container.name(), &["cat", "/etc/passwd"]);
    let expected_usr = match entry_for_id(&passwd, container.uid()) {
        Some(existing) => existing,
        None => {
            let planted = "preexisting_usr";
            run_capture(root_cmd(container.name()).args([
                "useradd",
                "-u",
                &container.uid().to_string(),
                "-g",
                &container.gid().to_string(),
                planted,
            ]));
            planted.to_string()
        }
    };

    container.bootstrap_user().await.expect("bootstrap_user");
    assert_eq!(
        container.group_name(),
        Some(expected_grp.as_str()),
        "bootstrap should reuse the pre-existing group at the host GID"
    );
    assert_eq!(
        container.user_name(),
        Some(expected_usr.as_str()),
        "bootstrap should reuse the pre-existing user at the host UID"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// The headline acceptance case: a bare image with no `shadow`, no `passwd`,
/// and nothing else added, bootstrapped and then exec'd into as the host user.
#[tokio::test]
async fn bootstrap_on_unadorned_alpine() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    container.bootstrap_user().await.expect("bootstrap_user");

    let user = container.user_name().expect("user name").to_string();
    let group = container.group_name().expect("group name").to_string();

    // Field-equivalent, not byte-identical: on podman 5 `--userns=keep-id`
    // auto-injects an entry at the host UID before bootstrap ever looks, and
    // bootstrap reuses it. Either way the databases must resolve the host ids
    // to the names the container reports.
    let passwd = root_stdout(container.name(), &["cat", "/etc/passwd"]);
    assert_eq!(
        entry_for_id(&passwd, container.uid()).as_deref(),
        Some(user.as_str()),
        "/etc/passwd should resolve the host uid to {user}:\n{passwd}"
    );
    // Whichever of the two wrote the entry, it names the `$HOME` checked
    // below: podman's own names the working directory, `/workspace`.
    assert_eq!(
        field_for_id(&passwd, container.uid(), 5).as_deref(),
        Some(format!("/home/{user}").as_str()),
        "the entry at the host uid should name /home/{user} as its home:\n{passwd}"
    );

    let groups = root_stdout(container.name(), &["cat", "/etc/group"]);
    assert_eq!(
        entry_for_id(&groups, container.gid()).as_deref(),
        Some(group.as_str()),
        "/etc/group should resolve the host gid to {group}:\n{groups}"
    );

    // `$HOME` is set by `exec_stdio` and must exist and be writable as the
    // host user -- that is what MCP servers land in.
    let mut child = container
        .exec_stdio(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "touch \"$HOME/probe\" && echo \"$HOME\"".to_string(),
            ],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_stdio home probe");
    assert_eq!(
        read_stdout(&mut child).await.trim(),
        format!("/home/{user}")
    );

    let owner = root_stdout(
        container.name(),
        &["stat", "-c", "%u %g", &format!("/home/{user}")],
    );
    assert_eq!(
        owner.trim(),
        format!("{} {}", container.uid(), container.gid()),
        "the home directory should belong to the host uid/gid"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// Strip any entry at `id` from one of the container's databases, in place so
/// the inode (and its owner and mode) survives. Undoes podman's `keep-id`
/// auto-injection, so that a test can watch bootstrap write an entry rather
/// than reuse one.
fn drop_entry(container: &str, path: &str, id: u32) {
    let script = format!("awk -F: '$3 != {id}' {path} > /tmp/db && cat /tmp/db > {path}");
    run_capture(root_cmd(container).args(["sh", "-c", &script]));
}

/// Append `lines` to `/etc/passwd` as the container's root.
fn plant_passwd(container: &str, lines: &[&str]) {
    for line in lines {
        let append = format!("echo '{line}' >> /etc/passwd");
        run_capture(root_cmd(container).args(["sh", "-c", &append]));
    }
}

/// A reused entry that names some other home has that one field rewritten,
/// in place, to the home bootstrap creates -- so `getpwnam` agrees with the
/// `$HOME` every exec gets. Planted in podman's own `keep-id` format, with a
/// line after it, so the test does not depend on whether this podman injects.
#[tokio::test]
async fn bootstrap_points_a_reused_entry_at_the_home_it_creates() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    let name = container.name().to_string();
    let (uid, gid) = (container.uid(), container.gid());

    drop_entry(&name, "/etc/passwd", uid);
    let planted = format!("reused:*:{uid}:{gid}:Planted User:/workspace:/bin/sh");
    let after = "after:x:4242:4242::/after:/bin/sh";
    plant_passwd(&name, &[&planted, after]);

    let stat = ["stat", "-c", "%i %u %g %a", "/etc/passwd"];
    let before = root_stdout(&name, &stat);
    container.bootstrap_user().await.expect("bootstrap_user");
    assert_eq!(container.user_name(), Some("reused"));

    let passwd = root_stdout(&name, &["cat", "/etc/passwd"]);
    let rewritten = format!("reused:*:{uid}:{gid}:Planted User:/home/reused:/bin/sh");
    assert!(
        passwd.lines().any(|line| line == rewritten),
        "only the home field should change, expected `{rewritten}` in:\n{passwd}"
    );
    assert_eq!(
        passwd.lines().last(),
        Some(after),
        "the line after the entry should survive:\n{passwd}"
    );
    assert_eq!(
        root_stdout(&name, &stat),
        before,
        "the rewrite must keep the file's inode, owner, and mode"
    );

    // `~reused` resolves through `getpwnam`, not `$HOME`.
    let mut child = container
        .exec_stdio(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo \"$HOME\" ~reused".to_string(),
            ],
            &ExecOptions::new(),
        )
        .await
        .expect("exec_stdio home probe");
    assert_eq!(
        read_stdout(&mut child).await.trim(),
        "/home/reused /home/reused"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// A borrowed container (`outrig mcp --attach`) may be running someone else's
/// processes, so its `/etc/passwd` is never truncated: the reused entry keeps
/// the home it names, and bootstrap still succeeds.
#[tokio::test]
async fn bootstrap_leaves_a_borrowed_containers_entry_alone() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let owner = start_alpine(host_ws.path(), None).await;
    let name = owner.name().to_string();
    let (uid, gid) = (owner.uid(), owner.gid());

    drop_entry(&name, "/etc/passwd", uid);
    let planted = format!("reused:*:{uid}:{gid}:Planted User:/workspace:/bin/sh");
    plant_passwd(&name, &[&planted]);
    let before = root_stdout(&name, &["cat", "/etc/passwd"]);

    let mut borrowed = Container::attach(
        name.clone(),
        owner.image_tag().clone(),
        Some((host_ws.path(), Path::new("/workspace"))),
        None,
    );
    borrowed.bootstrap_user().await.expect("bootstrap_user");
    assert_eq!(borrowed.user_name(), Some("reused"));
    assert_eq!(
        root_stdout(&name, &["cat", "/etc/passwd"]),
        before,
        "a borrowed container's /etc/passwd must not be rewritten"
    );

    drop(borrowed);
    owner.stop(Duration::from_secs(2)).await.expect("stop");
}

/// A reused name that is not a single path component would put the home
/// outside `/home` (`..` is `/` itself) or make directories on the way to it
/// (`a/b`). Bootstrap refuses it, naming the entry, and touches nothing.
#[tokio::test]
async fn bootstrap_refuses_a_reused_name_that_is_not_a_directory() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    let name = container.name().to_string();
    let (uid, gid) = (container.uid(), container.gid());

    for bad in ["..", "a/b"] {
        drop_entry(&name, "/etc/passwd", uid);
        let planted = format!("{bad}:x:{uid}:{gid}::/:/bin/sh");
        plant_passwd(&name, &[&planted]);

        let err = container.bootstrap_user().await.expect_err(bad);
        assert!(
            err.to_string().contains(&format!("{bad:?}")),
            "`{bad}`: the error should name the entry: {err}"
        );
        assert!(container.user_name().is_none(), "`{bad}`: not ready");

        let passwd = root_stdout(&name, &["cat", "/etc/passwd"]);
        assert!(
            passwd.lines().any(|line| line == planted),
            "`{bad}`: the entry should be left as it was:\n{passwd}"
        );
        let root = root_stdout(&name, &["stat", "-c", "%u %g", "/"]);
        assert_eq!(root.trim(), "0 0", "`{bad}`: / was chowned");
        let made = root_stdout(
            &name,
            &["sh", "-c", "test -e /home/a && echo yes || echo no"],
        );
        assert_eq!(made.trim(), "no", "`{bad}`: /home/a was created");
    }

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// What bootstrap writes when there is nothing to reuse: podman's auto-injected
/// entry is removed first, so the append path runs and can be checked against
/// the canonical `useradd`/`groupadd` forms.
#[tokio::test]
async fn bootstrap_writes_canonical_entries_when_absent() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    drop_entry(container.name(), "/etc/passwd", container.uid());
    drop_entry(container.name(), "/etc/group", container.gid());

    container.bootstrap_user().await.expect("bootstrap_user");
    let user = container.user_name().expect("user name").to_string();
    let group = container.group_name().expect("group name").to_string();

    let passwd = root_stdout(container.name(), &["cat", "/etc/passwd"]);
    let expected_entry = format!(
        "{user}:x:{}:{}::/home/{user}:/bin/sh",
        container.uid(),
        container.gid()
    );
    assert!(
        passwd.lines().any(|line| line == expected_entry),
        "expected `{expected_entry}` in /etc/passwd, got:\n{passwd}"
    );

    let groups = root_stdout(container.name(), &["cat", "/etc/group"]);
    let expected_group = format!("{group}:x:{}:", container.gid());
    assert!(
        groups.lines().any(|line| line == expected_group),
        "expected `{expected_group}` in /etc/group, got:\n{groups}"
    );

    // The written entry has to be usable, not just well-formed.
    let mut child = container
        .exec_stdio(&["id".to_string(), "-un".to_string()], &ExecOptions::new())
        .await
        .expect("exec_stdio id -un");
    assert_eq!(read_stdout(&mut child).await.trim(), user);

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// The direct path must leave `/etc/passwd` and `/etc/group` on their original
/// inodes: appended to, never replaced, so owner and mode survive.
#[tokio::test]
async fn bootstrap_preserves_etc_ownership_and_mode() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;

    let stat = ["stat", "-c", "%u %g %a", "/etc/passwd", "/etc/group"];
    let before = root_stdout(container.name(), &stat);
    container.bootstrap_user().await.expect("bootstrap_user");
    let after = root_stdout(container.name(), &stat);

    assert_eq!(
        before, after,
        "bootstrap must not change the owner or mode of the user databases"
    );
}

/// A home path the image already holds as something other than a directory
/// fails the bootstrap, naming the path, and is left alone -- a symlink in
/// particular must not carry the `chown` to whatever it points at.
#[tokio::test]
async fn bootstrap_refuses_a_home_that_is_not_a_directory() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;
    let name = container.name().to_string();

    // Pin the entry bootstrap will reuse, and with it the home path.
    drop_entry(&name, "/etc/passwd", container.uid());
    let entry = format!(
        "homeless:x:{}:{}::/home/homeless:/bin/sh",
        container.uid(),
        container.gid()
    );
    plant_passwd(&name, &[&entry]);

    // Each shape, and the file whose ownership a misdirected chown would take.
    for (plant, victim) in [
        ("touch /home/homeless", "/home/homeless"),
        (
            "rm /home/homeless && ln -s /etc/passwd /home/homeless",
            "/etc/passwd",
        ),
    ] {
        run_capture(root_cmd(&name).args(["sh", "-c", plant]));
        let err = container.bootstrap_user().await.expect_err(plant);
        assert!(
            err.to_string().contains("/home/homeless"),
            "`{plant}`: the error should name the home path: {err}"
        );
        assert!(container.user_name().is_none(), "`{plant}`: not ready");
        let owner = root_stdout(&name, &["stat", "-c", "%u %g", victim]);
        assert_eq!(owner.trim(), "0 0", "`{plant}`: {victim} was chowned");
    }

    container.stop(Duration::from_secs(2)).await.expect("stop");
}

/// The round-trip reduction, asserted rather than eyeballed: the whole
/// bootstrap issues no `podman exec` at all.
#[tokio::test]
async fn direct_bootstrap_issues_no_podman_exec() {
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let log_dir = tempfile::tempdir().expect("tempdir");
    let log = log_dir.path().join("container.log");
    let transcript = Transcript::create(&log, false).await.expect("transcript");
    let mut container = start_alpine(host_ws.path(), Some(transcript)).await;

    let before = fs::read_to_string(&log).expect("read transcript");
    container.bootstrap_user().await.expect("bootstrap_user");
    let after = fs::read_to_string(&log).expect("read transcript");
    let during = after.strip_prefix(&before).unwrap_or(&after).to_string();

    assert!(
        !during
            .lines()
            .any(|line| line.starts_with("[podman] $ podman exec")),
        "bootstrap should issue no `podman exec`, transcript said:\n{during}"
    );
    assert!(
        during.contains("[bootstrap]"),
        "bootstrap should record what it did, transcript said:\n{during}"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}
