//! #919: an external explicit session still writes a registry symlink under
//! the root. Refuse that write before creating a record or starting a build.

mod common;

use common::{GLOBAL_WITH_MODEL, run_outrig, utf8};

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
