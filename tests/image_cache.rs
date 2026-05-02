//! Pure-key tests for `outrig::image::CacheKey::compute`. These don't need
//! buildah or podman -- the cache-key machinery is hashing and subprocess work
//! against `git` and `tar`.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use outrig::image::CacheKey;

fn make_ctx(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (rel, contents) in files {
        let p = dir.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("create_dir_all");
        }
        std::fs::write(&p, contents).expect("write file");
    }
    dir
}

fn git_init(p: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(p)
            .args(args)
            .status()
            .expect("spawn git");
        assert!(status.success(), "git {args:?} failed in {p:?}");
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init", "--allow-empty"]);
}

async fn key(dockerfile: &Path, args: &BTreeMap<String, String>, ctx: &Path) -> String {
    CacheKey::compute(dockerfile, args, ctx)
        .await
        .expect("CacheKey::compute")
}

#[tokio::test]
async fn same_inputs_same_key() {
    let a = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("hello.txt", "hi\n")]);
    let b = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("hello.txt", "hi\n")]);
    let args = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);
    let ka = key(&a.path().join("Dockerfile"), &args, a.path()).await;
    let kb = key(&b.path().join("Dockerfile"), &args, b.path()).await;
    assert_eq!(ka, kb);
}

#[tokio::test]
async fn dockerfile_change_changes_key() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("file.txt", "x")]);
    let args = BTreeMap::new();
    let before = key(&ctx.path().join("Dockerfile"), &args, ctx.path()).await;

    std::fs::write(ctx.path().join("Dockerfile"), "FROM alpine:edge\n").unwrap();
    let after = key(&ctx.path().join("Dockerfile"), &args, ctx.path()).await;
    assert_ne!(before, after);
}

#[tokio::test]
async fn build_arg_change_changes_key() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let dockerfile = ctx.path().join("Dockerfile");

    let empty = BTreeMap::new();
    let with_arg = BTreeMap::from([("FOO".to_string(), "bar".to_string())]);
    let k_empty = key(&dockerfile, &empty, ctx.path()).await;
    let k_with = key(&dockerfile, &with_arg, ctx.path()).await;
    assert_ne!(k_empty, k_with);

    // Insertion order is irrelevant -- BTreeMap iterates in sorted key order.
    let mut a = BTreeMap::new();
    a.insert("A".to_string(), "1".to_string());
    a.insert("B".to_string(), "2".to_string());
    let mut b = BTreeMap::new();
    b.insert("B".to_string(), "2".to_string());
    b.insert("A".to_string(), "1".to_string());
    let ka = key(&dockerfile, &a, ctx.path()).await;
    let kb = key(&dockerfile, &b, ctx.path()).await;
    assert_eq!(ka, kb);
}

#[tokio::test]
async fn context_file_change_changes_key() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("data.txt", "v1")]);
    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let before = key(&dockerfile, &args, ctx.path()).await;

    std::fs::write(ctx.path().join("data.txt"), "v2").unwrap();
    let after = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(before, after);
}

#[tokio::test]
async fn gitignored_file_does_not_affect_key_when_in_git() {
    let ctx_a = make_ctx(&[
        ("Dockerfile", "FROM alpine\n"),
        ("src/lib.rs", "// hi\n"),
        (".gitignore", "ignored.log\n"),
    ]);
    git_init(ctx_a.path());

    let ctx_b = make_ctx(&[
        ("Dockerfile", "FROM alpine\n"),
        ("src/lib.rs", "// hi\n"),
        (".gitignore", "ignored.log\n"),
    ]);
    git_init(ctx_b.path());
    std::fs::write(ctx_b.path().join("ignored.log"), "stray artifact\n").unwrap();

    let args = BTreeMap::new();
    let ka = key(&ctx_a.path().join("Dockerfile"), &args, ctx_a.path()).await;
    let kb = key(&ctx_b.path().join("Dockerfile"), &args, ctx_b.path()).await;
    assert_eq!(ka, kb, "untracked ignored file must not affect the key");

    // Force-add the previously-ignored file: now it's tracked, key must change.
    let status = Command::new("git")
        .current_dir(ctx_b.path())
        .args(["add", "-f", "ignored.log"])
        .status()
        .unwrap();
    assert!(status.success());
    let kb2 = key(&ctx_b.path().join("Dockerfile"), &args, ctx_b.path()).await;
    assert_ne!(kb, kb2, "tracking a new file must change the key");
}

#[tokio::test]
async fn non_git_context_uses_tar() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("a.txt", "alpha")]);
    assert!(
        !ctx.path().join(".git").exists(),
        "fixture must not be a git repo"
    );

    let args = BTreeMap::new();
    let dockerfile = ctx.path().join("Dockerfile");
    let k1 = key(&dockerfile, &args, ctx.path()).await;
    let k2 = key(&dockerfile, &args, ctx.path()).await;
    assert_eq!(k1, k2, "tar-based hash must be deterministic across runs");

    std::fs::write(ctx.path().join("a.txt"), "beta").unwrap();
    let k3 = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(k1, k3);
}

#[tokio::test]
async fn tar_path_key_is_mtime_independent() {
    // Two non-git contexts with byte-identical content. Bump the mtime on
    // every entry of one of them by a year so any naive tar invocation would
    // produce different bytes. With --mtime=UTC 1970-01-01 the archive is
    // mtime-stripped, so the hash must still match.
    let a = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("payload", "data")]);
    let b = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("payload", "data")]);
    for entry in ["Dockerfile", "payload"] {
        let status = Command::new("touch")
            .args(["-d", "2030-06-15T12:00:00"])
            .arg(b.path().join(entry))
            .status()
            .expect("spawn touch");
        assert!(status.success(), "touch failed for {entry}");
    }
    let args = BTreeMap::new();
    let ka = key(&a.path().join("Dockerfile"), &args, a.path()).await;
    let kb = key(&b.path().join("Dockerfile"), &args, b.path()).await;
    assert_eq!(
        ka, kb,
        "tar-path hash must ignore filesystem mtimes (so fresh clones cache-hit)"
    );
}

#[tokio::test]
async fn key_length_is_16_hex_chars() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let args = BTreeMap::new();
    let k = key(&ctx.path().join("Dockerfile"), &args, ctx.path()).await;
    assert_eq!(k.len(), 16, "key was {k:?}");
    assert!(
        k.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
        "key must be lowercase hex, got {k:?}"
    );
}
