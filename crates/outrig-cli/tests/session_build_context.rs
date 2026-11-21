//! #919: an external explicit session still writes a registry symlink under
//! the root. Refuse that write before creating a record or starting a build.

mod common;

use common::{GLOBAL_WITH_MODEL, run_outrig, run_outrig_with_env, utf8};

#[tokio::test]
async fn external_session_checks_the_registry_entry_before_creation() {
    for (git, ignore, external_root, linked_root, collision) in [
        (false, "", false, false, true),
        (true, "", false, false, true),
        (true, "/roots/*/\n", false, false, true),
        (true, "", false, true, true),
        (true, "/roots/*\n", false, false, false),
        (true, "/roots/\n", false, false, false),
        (false, "", true, false, false),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agents = repo.join(".agents/outrig");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("config.toml"),
            "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n"
        ).unwrap();
        std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(repo.join(".gitignore"), ignore).unwrap();
        if git {
            assert!(
                std::process::Command::new("git")
                    .args(["init", "-q"])
                    .arg(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let global = tmp.path().join("global.toml");
        std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
        let explicit = tmp.path().join("external-session");
        let actual_root = if external_root {
            tmp.path().join("roots")
        } else {
            repo.join("roots")
        };
        let root = if linked_root {
            std::fs::create_dir(&actual_root).unwrap();
            let link = tmp.path().join("root-link");
            std::os::unix::fs::symlink(&actual_root, &link).unwrap();
            link
        } else {
            actual_root.clone()
        };
        let (ok, stderr) = run_outrig(
            &repo,
            &[
                "--global-config",
                utf8(&global),
                "--session-root",
                utf8(&root),
                "run",
                "--session-dir",
                utf8(&explicit),
            ],
        )
        .await;
        assert!(!ok, "the stub runtime stops an allowed run too: {stderr}");
        assert_eq!(
            stderr.contains("inside the build context"),
            collision,
            "{stderr}"
        );
        if collision {
            assert!(stderr.contains("session registry entry"), "{stderr}");
            assert!(stderr.contains("pass --session-root outside"), "{stderr}");
            assert!(
                stderr.contains("file-shaped entries in .gitignore"),
                "{stderr}"
            );
            assert!(!stderr.contains("--session-dir"), "{stderr}");
            assert!(stderr.contains(&format!("{}/", root.display())), "{stderr}");
            assert!(!stderr.contains("computing image tag"), "{stderr}");
            if linked_root {
                assert_eq!(std::fs::read_dir(&actual_root).unwrap().count(), 0);
            } else {
                assert!(
                    !root.exists(),
                    "no registry entry or probe parent left behind"
                );
            }
            assert!(
                !explicit.exists(),
                "refuse before creating the session record"
            );
        } else {
            assert!(explicit.join("session.json").exists(), "{stderr}");
            let entries = std::fs::read_dir(&root)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert!(entries[0].path().is_symlink());
        }
    }
}

/// #921: refuse the alias route before hashing, building, or session writes,
/// for auto-allocated directories, explicit ones, and registry entries alike.
#[tokio::test]
async fn ignored_session_paths_reached_by_alias_are_refused_before_creation() {
    for mode in ["auto", "explicit", "registry"] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agents = repo.join(".agents/outrig");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("config.toml"),
            "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n"
        ).unwrap();
        std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "/sessions/\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::os::unix::fs::symlink("sessions", repo.join("alias")).unwrap();
        let global = tmp.path().join("global.toml");
        std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
        let root = if mode == "explicit" {
            tmp.path().join("registry")
        } else {
            repo.join("sessions")
        };
        let explicit = if mode == "registry" {
            tmp.path().join("external-session")
        } else {
            repo.join("sessions/new")
        };
        let mut args = vec![
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&root),
            "run",
        ];
        if mode != "auto" {
            args.extend(["--session-dir", utf8(&explicit)]);
        }
        let (ok, stderr) = run_outrig(&repo, &args).await;
        assert!(!ok, "{mode}: {stderr}");
        assert!(stderr.contains("is reached by symlink"), "{mode}: {stderr}");
        assert!(
            stderr.contains(utf8(&repo.join("alias"))),
            "{mode}: {stderr}"
        );
        assert!(
            stderr.contains(utf8(&repo.join("sessions"))),
            "{mode}: {stderr}"
        );
        assert!(!stderr.contains("computing image tag"), "{mode}: {stderr}");
        assert!(
            !repo.join("sessions").exists(),
            "probe left session paths behind"
        );
        assert!(!root.exists(), "registry was created");
        assert!(!explicit.exists(), "session record was created");
    }
}

/// Review of #921: unrelated dangling or unresolvable referents do not make
/// an ignored explicit session part of the key.
#[tokio::test]
async fn ignored_sessions_with_unwritten_or_unresolvable_aliases_reach_startup() {
    for target in [
        "sessions/new/unused",
        "Dockerfile/x",
        "sessions/new/logs/container.log/unused",
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agents = repo.join(".agents/outrig");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("config.toml"),
            "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n"
        ).unwrap();
        std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "/sessions/\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        std::os::unix::fs::symlink(target, repo.join("alias")).unwrap();
        let global = tmp.path().join("global.toml");
        std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
        let root = tmp.path().join("registry");
        let explicit = repo.join("sessions/new");
        let (ok, stderr) = run_outrig(
            &repo,
            &[
                "--global-config",
                utf8(&global),
                "--session-root",
                utf8(&root),
                "-v",
                "run",
                "--session-dir",
                utf8(&explicit),
            ],
        )
        .await;
        assert!(!ok, "the stub runtime stops an allowed run: {stderr}");
        assert!(stderr.contains("image tag computed"), "{target}: {stderr}");
        assert!(
            !stderr.contains("is reached by symlink"),
            "{target}: {stderr}"
        );
        assert!(explicit.join("session.json").exists(), "{target}: {stderr}");
        assert!(
            explicit.join("logs/container.log").is_file(),
            "{target}: {stderr}"
        );
    }
}

/// #996: a file-shaped registry entry ignored by `/roots/*` would still be
/// hashed through an admitted alias to its parent. Refuse before writing it.
#[tokio::test]
async fn ignored_registry_entry_reached_through_parent_alias_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/roots/*\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    std::os::unix::fs::symlink("roots", repo.join("alias")).unwrap();
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let external = tmp.path().join("external-session");
    let root = repo.join("roots");
    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&root),
            "run",
            "--session-dir",
            utf8(&external),
        ],
    )
    .await;
    assert!(!ok, "{stderr}");
    assert!(stderr.contains("is reached by symlink"), "{stderr}");
    assert!(stderr.contains(utf8(&repo.join("alias"))), "{stderr}");
    assert!(!stderr.contains("computing image tag"), "{stderr}");
    assert!(!root.exists(), "probe left registry behind");
    assert!(!external.exists(), "session record was created");
}

/// #1002: the first auto session creates a formerly absent, unignored root.
/// It must be present before the tag is computed, not appear between keying
/// and the pre-build rehash. The stub buildah refuses the build itself.
#[tokio::test]
async fn absent_unignored_session_root_reaches_build_on_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/sessions/*\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let root = repo.join("sessions");
    assert!(!root.exists());
    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&root),
            "run",
        ],
    )
    .await;
    assert!(!ok, "stub buildah must stop this launch: {stderr}");
    assert!(stderr.contains("ensuring image"), "{stderr}");
    assert!(
        stderr.contains("buildah"),
        "must reach stub buildah: {stderr}"
    );
    assert!(!stderr.contains("BuildContextChanged"), "{stderr}");
    assert!(root.is_dir());
}

/// Git can list from the context root while discovery from cache/ is blocked
/// by GIT_CEILING_DIRECTORIES. The hasher must classify the ignored subdir
/// from the same root and must not key its contents or mode.
#[tokio::test]
async fn ignored_subdirectory_stays_excluded_under_git_ceiling() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/cache/sub/\n").unwrap();
    std::fs::create_dir(repo.join("cache")).unwrap();
    std::fs::write(repo.join("cache/keep"), "keep").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["-C", utf8(&repo), "add", "cache/keep"])
            .status()
            .unwrap()
            .success()
    );
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let root = tmp.path().join("sessions");
    let args = &[
        "--global-config",
        utf8(&global),
        "--session-root",
        utf8(&root),
        "run",
    ];
    let env = [("GIT_CEILING_DIRECTORIES", repo.as_path())];
    let run = || run_outrig_with_env(&repo, args, &env);
    let (ok, before) = run().await;
    assert!(!ok && before.contains("buildah"), "{before}");
    std::fs::create_dir(repo.join("cache/sub")).unwrap();
    std::fs::write(repo.join("cache/sub/file"), "data").unwrap();
    let (ok, after) = run().await;
    assert!(!ok && after.contains("buildah"), "{after}");
    let tag = |text: &str| {
        text.lines()
            .find_map(|line| line.split("image tag computed: ").nth(1))
            .expect("computed tag")
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    };
    assert_eq!(tag(&before), tag(&after));
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(repo.join("cache/sub"))
            .unwrap()
            .permissions(),
    );
    std::fs::set_permissions(
        repo.join("cache/sub"),
        std::os::unix::fs::PermissionsExt::from_mode(mode ^ 0o010),
    )
    .unwrap();
    let (_, changed) = run().await;
    assert_eq!(tag(&before), tag(&changed));
}

/// A warm or cold tag lookup uses one bulk ignore query per tree, not one
/// subprocess per ignored directory left by earlier sessions.
#[tokio::test]
async fn ignore_query_count_does_not_grow_with_session_history() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/sessions/*\n").unwrap();
    std::fs::create_dir(repo.join("sessions")).unwrap();
    std::fs::create_dir(repo.join("sessions/old-initial")).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let root = tmp.path().join("external-sessions");
    let trace = tmp.path().join("git-trace.json");
    let args = &[
        "--global-config",
        utf8(&global),
        "--session-root",
        utf8(&root),
        "run",
    ];
    let env = [("GIT_TRACE2_EVENT", trace.as_path())];
    let run = || run_outrig_with_env(&repo, args, &env);
    let count = || {
        std::fs::read_to_string(&trace)
            .unwrap()
            .lines()
            .filter(|line| line.contains("\"argv\"") && line.contains("--stdin"))
            .count()
    };
    let (_, before) = run().await;
    assert!(before.contains("buildah"), "{before}");
    let one = count();
    assert!(one > 0);
    for i in 0..40 {
        std::fs::create_dir(repo.join(format!("sessions/old-{i}"))).unwrap();
    }
    let (_, after) = run().await;
    assert!(after.contains("buildah"), "{after}");
    assert_eq!(
        count() - one,
        one,
        "ignored siblings must not add Git probes"
    );
}

/// #1002: an ignored explicit session may have an absent unignored parent in
/// the context. Create only that parent before keying, not the claimed leaf.
#[tokio::test]
async fn absent_explicit_session_parent_reaches_build_on_cache_miss() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/sessions/*\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let root = tmp.path().join("registry");
    let explicit = repo.join("sessions/new");
    assert!(!explicit.parent().unwrap().exists());
    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&root),
            "run",
            "--session-dir",
            utf8(&explicit),
        ],
    )
    .await;
    assert!(!ok && stderr.contains("buildah"), "{stderr}");
    assert!(!stderr.contains("BuildContextChanged"), "{stderr}");
    assert!(explicit.join("session.json").exists());
}

/// A registry spelling can traverse an initially absent unignored prefix
/// before `..` resolves it inside the ignored explicit session directory.
#[tokio::test]
async fn lexical_registry_prefixes_exist_before_tag_computation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let agents = repo.join(".agents/outrig");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("config.toml"),
        "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "/sessions/*\n/staging/*\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap()
            .success()
    );
    let global = tmp.path().join("global.toml");
    std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let explicit = repo.join("sessions/new");
    let root = repo.join("staging/transit/../../sessions/new/registry");
    assert!(!repo.join("staging").exists());
    assert!(!repo.join("sessions").exists());
    let (ok, stderr) = run_outrig(
        &repo,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&root),
            "run",
            "--session-dir",
            utf8(&explicit),
        ],
    )
    .await;
    assert!(!ok && stderr.contains("buildah"), "{stderr}");
    assert!(!stderr.contains("BuildContextChanged"), "{stderr}");
    assert!(repo.join("staging").is_dir());
    assert!(explicit.join("session.json").exists());
}

/// Preparing a registry under an empty explicit directory would make the
/// store reject that directory as nonempty before claiming it.
#[tokio::test]
async fn registry_under_explicit_directory_does_not_preempt_emptiness_check() {
    for (existing, lexical) in [(false, false), (true, false), (false, true), (true, true)] {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let agents = repo.join(".agents/outrig");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("config.toml"),
            "default-image = \"tool\"\n[images.tool]\ndockerfile = \"Dockerfile\"\ncontext = \".\"\n",
        ).unwrap();
        std::fs::write(repo.join("Dockerfile"), "FROM scratch\n").unwrap();
        let global = tmp.path().join("global.toml");
        std::fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
        let explicit = if lexical {
            tmp.path().join("run/new")
        } else {
            tmp.path().join("run")
        };
        if existing {
            std::fs::create_dir_all(&explicit).unwrap();
        }
        let root = if lexical {
            explicit.join("inner/../../new/registry")
        } else {
            explicit.join("registry")
        };
        let (ok, stderr) = run_outrig(
            &repo,
            &[
                "--global-config",
                utf8(&global),
                "--session-root",
                utf8(&root),
                "run",
                "--session-dir",
                utf8(&explicit),
            ],
        )
        .await;
        assert!(!ok && stderr.contains("buildah"), "{stderr}");
        assert!(!stderr.contains("not empty"), "{stderr}");
        assert!(explicit.join("session.json").exists());
    }
}
