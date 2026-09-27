//! Pure-key tests for `image::CacheKey::compute`. These don't need
//! buildah or podman -- the cache-key machinery is hashing and subprocess work
//! against `git` and `tar`.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use super::{
    CacheKey, ImageConfig, ImageTag, McpServerSpec, UNNAMED_IMAGE, compute_tag, compute_tag_for,
};

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

fn git(p: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(p)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed in {p:?}");
}

/// Make `p` a repository with everything already in it committed. Files
/// written afterwards stay untracked.
fn git_init(p: &Path) {
    git(p, &["init", "-q", "-b", "main"]);
    git(p, &["config", "user.email", "test@example.com"]);
    git(p, &["config", "user.name", "test"]);
    git(p, &["config", "commit.gpgsign", "false"]);
    git(p, &["add", "-A"]);
    git(p, &["commit", "-q", "-m", "init", "--allow-empty"]);
}

/// Pin that `rel` is untracked but not ignored, so a test of that case cannot
/// quietly become a second copy of the gitignore one. `check-ignore` exits 1
/// for "not ignored" and 128 on error, so only 1 passes.
fn assert_not_ignored(repo: &Path, rel: &str) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(["check-ignore", "-q", rel])
        .status()
        .expect("spawn git");
    assert_eq!(status.code(), Some(1), "{rel} must not be gitignored");
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
async fn label_change_changes_key() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let labels_a = BTreeMap::from([("org.outrig.mcp".to_string(), "{}".to_string())]);
    let labels_b = BTreeMap::from([(
        "org.outrig.mcp".to_string(),
        r#"{"fs":["mcp-server-filesystem","/workspace"]}"#.to_string(),
    )]);

    let ka = CacheKey::compute_with_labels(&dockerfile, &args, ctx.path(), &labels_a)
        .await
        .expect("CacheKey::compute_with_labels");
    let kb = CacheKey::compute_with_labels(&dockerfile, &args, ctx.path(), &labels_b)
        .await
        .expect("CacheKey::compute_with_labels");

    assert_ne!(ka, kb);
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
    git(ctx_b.path(), &["add", "-f", "ignored.log"]);
    let kb2 = key(&ctx_b.path().join("Dockerfile"), &args, ctx_b.path()).await;
    assert_ne!(kb, kb2, "tracking a new file must change the key");
}

/// buildah is handed the whole context directory, so a file nobody has
/// committed yet is as much a build input as a tracked one.
#[tokio::test]
async fn untracked_nonignored_file_changes_key() {
    let ctx = make_ctx(&[(
        "Dockerfile",
        "FROM scratch\nCOPY generated.txt /generated.txt\n",
    )]);
    git_init(ctx.path());
    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let absent = key(&dockerfile, &args, ctx.path()).await;

    std::fs::write(ctx.path().join("generated.txt"), "v1").unwrap();
    assert_not_ignored(ctx.path(), "generated.txt");
    let added = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(
        absent, added,
        "adding an untracked file must change the key"
    );

    std::fs::write(ctx.path().join("generated.txt"), "v2-different").unwrap();
    let edited = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(
        added, edited,
        "changing an untracked, non-ignored context file must change the key"
    );
}

/// A tracked file removed without `git rm` is still in the index, but not in
/// what buildah receives. It hashes as absent rather than failing the key.
#[tokio::test]
async fn deleted_tracked_file_is_skipped() {
    let a = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("gone.txt", "x")]);
    let b = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    git_init(a.path());
    git_init(b.path());
    std::fs::remove_file(a.path().join("gone.txt")).unwrap();

    let args = BTreeMap::new();
    let ka = key(&a.path().join("Dockerfile"), &args, a.path()).await;
    let kb = key(&b.path().join("Dockerfile"), &args, b.path()).await;
    assert_eq!(ka, kb, "the key follows the worktree, not the index");
}

/// `COPY` keeps names and permission bits, so a rename or a `chmod +x` on an
/// entrypoint changes the image and must change the key.
#[tokio::test]
async fn path_and_mode_are_part_of_the_key() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("entry.sh", "true\n")]);
    git_init(ctx.path());
    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();

    let original = key(&dockerfile, &args, ctx.path()).await;
    std::fs::set_permissions(
        ctx.path().join("entry.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let executable = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(original, executable, "a mode change must change the key");

    std::fs::rename(ctx.path().join("entry.sh"), ctx.path().join("start.sh")).unwrap();
    let renamed = key(&dockerfile, &args, ctx.path()).await;
    assert_ne!(executable, renamed, "a rename must change the key");
}

#[tokio::test]
async fn subdirectory_context_hashes_its_own_files() {
    let repo = make_ctx(&[
        ("outside.txt", "o"),
        ("images/coding/Dockerfile", "FROM alpine\n"),
        ("images/coding/tracked.txt", "t"),
    ]);
    git_init(repo.path());
    let ctx = repo.path().join("images/coding");
    std::fs::write(ctx.join("untracked.txt"), "v1").unwrap();
    let dockerfile = ctx.join("Dockerfile");
    let args = BTreeMap::new();

    let before = key(&dockerfile, &args, &ctx).await;
    std::fs::write(repo.path().join("outside.txt"), "changed").unwrap();
    std::fs::write(repo.path().join("stray.txt"), "new").unwrap();
    assert_eq!(
        before,
        key(&dockerfile, &args, &ctx).await,
        "files outside the context must not affect the key"
    );

    std::fs::write(ctx.join("untracked.txt"), "v2").unwrap();
    assert_ne!(
        before,
        key(&dockerfile, &args, &ctx).await,
        "an untracked file inside a subdirectory context must count"
    );
}

/// git lists an untracked nested repository as one `dir/` entry. It is hashed
/// by the same rule, in its own repository, so its own `.gitignore` applies.
#[tokio::test]
async fn nested_repo_in_context_is_hashed() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    // The outer commit comes first: `add -A` over an existing nested
    // repository would record it as a gitlink instead of leaving it untracked.
    git_init(ctx.path());
    let lib = ctx.path().join("vendor/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("lib.txt"), "v1").unwrap();
    std::fs::write(lib.join(".gitignore"), "build/\n").unwrap();
    git_init(&lib);

    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let before = key(&dockerfile, &args, ctx.path()).await;

    std::fs::create_dir_all(lib.join("build")).unwrap();
    std::fs::write(lib.join("build/out.o"), "artifact").unwrap();
    assert_eq!(
        before,
        key(&dockerfile, &args, ctx.path()).await,
        "a file the nested repository ignores must not count"
    );

    std::fs::write(lib.join("lib.txt"), "v2").unwrap();
    assert_ne!(
        before,
        key(&dockerfile, &args, ctx.path()).await,
        "an edit inside a nested repository must change the key"
    );
}

/// An uninitialized submodule is a gitlink over a directory git does not treat
/// as a repository -- no `.git`, or one git rejects, which it looks past to
/// the enclosing repository. Whatever is in that directory reaches buildah,
/// so it counts.
#[tokio::test]
async fn uninitialized_submodule_is_hashed() {
    for marker in ["none", "empty .git directory", "dangling .git link"] {
        let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
        git_init(ctx.path());
        git(
            ctx.path(),
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                "160000,0123456789abcdef0123456789abcdef01234567,sub",
            ],
        );
        let sub = ctx.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        match marker {
            "empty .git directory" => std::fs::create_dir(sub.join(".git")).unwrap(),
            "dangling .git link" => {
                std::os::unix::fs::symlink("missing", sub.join(".git")).unwrap()
            }
            _ => {}
        }

        let dockerfile = ctx.path().join("Dockerfile");
        let args = BTreeMap::new();
        let before = key(&dockerfile, &args, ctx.path()).await;
        std::fs::write(sub.join("stray.txt"), "x").unwrap();
        assert_ne!(
            before,
            key(&dockerfile, &args, ctx.path()).await,
            "{marker}: a file inside the submodule directory must change the key"
        );
    }
}

/// `COPY <link> <dest>` copies what the link points at, resolved with the
/// context as `/`: an absolute target and a `..` past the top both stay
/// inside it. That may be a file or a whole directory git never lists, like
/// the refs under `.git`.
#[tokio::test]
async fn symlink_keys_what_it_resolves_to() {
    for target in [
        ".git/refs/heads/main",
        "/.git/refs/heads/main",
        "../../.git/refs/heads/main",
        ".git/refs",
        "/.git/refs/heads",
    ] {
        let ctx = make_ctx(&[("Dockerfile", "FROM scratch\nCOPY revision /revision\n")]);
        git_init(ctx.path());
        std::os::unix::fs::symlink(target, ctx.path().join("revision")).unwrap();

        let dockerfile = ctx.path().join("Dockerfile");
        let args = BTreeMap::new();
        let before = key(&dockerfile, &args, ctx.path()).await;
        git(ctx.path(), &["commit", "-q", "--allow-empty", "-m", "next"]);
        assert_ne!(
            before,
            key(&dockerfile, &args, ctx.path()).await,
            "moving the ref that `revision -> {target}` copies must change the key"
        );
    }
}

/// `COPY <link> <dest>` keeps the permission bits of the file it copies, and
/// that file is not otherwise in the key when git does not list it.
#[tokio::test]
async fn symlink_keys_the_mode_of_what_it_resolves_to() {
    let ctx = make_ctx(&[("Dockerfile", "FROM scratch\nCOPY cfg /cfg\n")]);
    git_init(ctx.path());
    let stamp = ctx.path().join(".git/stamp");
    std::fs::write(&stamp, "x").unwrap();
    std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(".git/stamp", ctx.path().join("cfg")).unwrap();

    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let before = key(&dockerfile, &args, ctx.path()).await;
    std::fs::set_permissions(&stamp, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_ne!(
        before,
        key(&dockerfile, &args, ctx.path()).await,
        "a mode change on the file a link resolves to must change the key"
    );
}

/// buildah resolves every `COPY` source against the build context, so a link
/// inside a nested repository resolves there too, not in the nested root.
#[tokio::test]
async fn symlink_in_a_nested_repo_resolves_against_the_context() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    git_init(ctx.path());
    let lib = ctx.path().join("vendor/lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("lib.txt"), "v1").unwrap();
    git_init(&lib);
    std::os::unix::fs::symlink("/.git/refs/heads/main", lib.join("revision")).unwrap();

    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let before = key(&dockerfile, &args, ctx.path()).await;
    git(ctx.path(), &["commit", "-q", "--allow-empty", "-m", "next"]);
    assert_ne!(
        before,
        key(&dockerfile, &args, ctx.path()).await,
        "moving the context's own ref must change the key"
    );
}

/// A link inside a tree another link copies is followed too: `COPY
/// meta/selected` resolves both links, and lands on a file git never lists.
#[tokio::test]
async fn symlink_inside_a_linked_directory_keys_what_it_resolves_to() {
    let ctx = make_ctx(&[("Dockerfile", "FROM scratch\nCOPY meta/selected /selected\n")]);
    git_init(ctx.path());
    let buildmeta = ctx.path().join(".git/buildmeta");
    std::fs::create_dir(&buildmeta).unwrap();
    std::fs::write(ctx.path().join(".git/stamp"), "v1").unwrap();
    std::os::unix::fs::symlink("/.git/stamp", buildmeta.join("selected")).unwrap();
    std::os::unix::fs::symlink(".git/buildmeta", ctx.path().join("meta")).unwrap();

    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let before = key(&dockerfile, &args, ctx.path()).await;
    std::fs::write(ctx.path().join(".git/stamp"), "v2").unwrap();
    assert_ne!(
        before,
        key(&dockerfile, &args, ctx.path()).await,
        "an edit to the file a nested link resolves to must change the key"
    );
}

/// buildah cleans a link's target before resolving any of it -- a relative
/// one joined to the link's directory first -- so `alias/..` cancels even
/// though `alias` is a link: `revision` names `./stamp.txt`, not
/// `data/stamp.txt`.
#[tokio::test]
async fn symlink_target_is_cleaned_before_it_is_resolved() {
    for target in ["alias/../stamp.txt", "/alias/../stamp.txt"] {
        let ctx = make_ctx(&[
            ("Dockerfile", "FROM scratch\nCOPY revision /revision\n"),
            (".gitignore", "stamp.txt\n"),
            ("stamp.txt", "v1"),
            ("data/stamp.txt", "inner"),
            ("data/sub/.keep", ""),
        ]);
        std::os::unix::fs::symlink("data/sub", ctx.path().join("alias")).unwrap();
        std::os::unix::fs::symlink(target, ctx.path().join("revision")).unwrap();
        git_init(ctx.path());

        let dockerfile = ctx.path().join("Dockerfile");
        let args = BTreeMap::new();
        let before = key(&dockerfile, &args, ctx.path()).await;
        std::fs::write(ctx.path().join("stamp.txt"), "v2").unwrap();
        assert_ne!(
            before,
            key(&dockerfile, &args, ctx.path()).await,
            "`revision -> {target}` copies ./stamp.txt, so editing it must change the key"
        );
    }
}

/// A link that leads back into what is already being hashed -- a pair naming
/// each other, a link to the context itself -- ends there instead of hanging
/// or failing the key.
#[tokio::test]
async fn symlink_loops_end() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n"), ("d/f.txt", "x")]);
    git_init(ctx.path());
    std::os::unix::fs::symlink("b", ctx.path().join("a")).unwrap();
    std::os::unix::fs::symlink("a", ctx.path().join("b")).unwrap();
    std::os::unix::fs::symlink(".", ctx.path().join("here")).unwrap();
    std::os::unix::fs::symlink("..", ctx.path().join("d/up")).unwrap();

    let dockerfile = ctx.path().join("Dockerfile");
    let args = BTreeMap::new();
    let k1 = key(&dockerfile, &args, ctx.path()).await;
    let k2 = key(&dockerfile, &args, ctx.path()).await;
    assert_eq!(k1, k2);
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

fn build_cfg(ctx: &Path) -> ImageConfig {
    ImageConfig::from_dockerfile(ctx.join("Dockerfile"), ctx)
}

#[tokio::test]
async fn named_build_tag_uses_image_config_name_as_repo() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let cfg = build_cfg(ctx.path());

    // Empty repo_root: dockerfile/context are already absolute, so join is a no-op.
    let tag = compute_tag_for("outrig-standard", &cfg, Path::new(""))
        .await
        .expect("compute_tag_for");

    let (repo, hash) = tag
        .as_str()
        .split_once(':')
        .expect("tag has repo:hash form");
    assert_eq!(repo, "outrig-standard");
    assert_eq!(
        hash.len(),
        16,
        "tag part is the 16-hex cache key, got {hash:?}"
    );
}

#[tokio::test]
async fn named_build_tag_tracks_repo_mcp_labels() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let mut with_fs = build_cfg(ctx.path());
    with_fs.mcp.insert(
        "fs".to_string(),
        McpServerSpec::Short(vec![
            "mcp-server-filesystem".to_string(),
            "/workspace".to_string(),
        ]),
    );
    let mut with_git = build_cfg(ctx.path());
    with_git.mcp.insert(
        "git".to_string(),
        McpServerSpec::Short(vec!["mcp-server-git".to_string()]),
    );

    let fs_tag = compute_tag_for("outrig-standard", &with_fs, Path::new(""))
        .await
        .expect("compute_tag_for");
    let git_tag = compute_tag_for("outrig-standard", &with_git, Path::new(""))
        .await
        .expect("compute_tag_for");

    assert_ne!(fs_tag, git_tag);
}

#[tokio::test]
async fn unnamed_build_tag_falls_back_to_outrig_cache() {
    let ctx = make_ctx(&[("Dockerfile", "FROM alpine\n")]);
    let cfg = build_cfg(ctx.path());

    let named = compute_tag_for(UNNAMED_IMAGE, &cfg, Path::new(""))
        .await
        .expect("compute_tag_for");
    let unnamed = compute_tag(&cfg, Path::new("")).await.expect("compute_tag");

    assert_eq!(named, unnamed);
    assert!(
        named.as_str().starts_with("outrig-cache:"),
        "nameless path keeps the outrig-cache repository, got {}",
        named.as_str()
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

#[test]
fn image_tag_reads_back_what_it_was_built_from() {
    let from_str = ImageTag::new("docker.io/library/alpine:3.20");
    let from_string = ImageTag::from("docker.io/library/alpine:3.20".to_string());

    assert_eq!(from_str, from_string);
    assert_eq!(from_str.as_str(), "docker.io/library/alpine:3.20");
    assert_eq!(from_str.to_string(), "docker.io/library/alpine:3.20");
    assert_eq!(from_string.into_string(), "docker.io/library/alpine:3.20");
}

/// A temporary tag stays inside the 128 characters an engine accepts, however
/// long the caller's ref is.
///
/// `build_standalone` hands this whatever ref the project declared, and a
/// 128-character tag is valid. Echoing it back under a nonce would push the
/// *build* over the limit and fail a build that used to succeed, so the
/// echoed part is bounded.
#[test]
fn a_temporary_tag_fits_an_engine_tag_however_long_the_caller_ref_is() {
    const OCI_TAG_LIMIT: usize = 128;

    for key_len in [1, 16, 32, 33, OCI_TAG_LIMIT] {
        let key = "k".repeat(key_len);
        let temp = super::temporary_build_tag(&super::ImageTag::new(format!("rust-dev:{key}")));
        let (repo, tag) = temp
            .as_str()
            .rsplit_once(':')
            .expect("a temporary tag always has a tag part");
        assert_eq!(repo, "rust-dev");
        assert!(
            tag.len() <= OCI_TAG_LIMIT,
            "a {key_len}-character key produced a {}-character tag: {tag}",
            tag.len()
        );
        assert!(
            tag.starts_with("outrig-tmp-"),
            "the temporary shape is what every cleanup selects on, got {tag}"
        );
    }
}

/// A registry port is not a tag separator, and a ref with no tag keeps its
/// own repository rather than landing in `outrig-cache`.
#[test]
fn a_temporary_tag_splits_a_ref_where_the_engine_would() {
    let cases = [
        ("rust-dev:1.0", "rust-dev"),
        ("localhost:5000/team/img", "localhost:5000/team/img"),
        ("rust-dev", "rust-dev"),
    ];
    for (input, expected_repo) in cases {
        let temp = super::temporary_build_tag(&super::ImageTag::new(input));
        let (repo, tag) = temp.as_str().rsplit_once(':').expect("tagged");
        assert_eq!(repo, expected_repo, "for {input}");
        assert!(!tag.contains('/'), "a tag may not contain a slash: {tag}");
    }
}
