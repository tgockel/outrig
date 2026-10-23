use std::fs;
use std::os::unix::fs::symlink;
use std::path::PathBuf;

use super::{Instructions, MAX_BYTES, load};
use crate::config::RepoInstructions;
use crate::python::host::FileText;
use crate::python::testing::{start_on_host, within};

/// A workspace holding `files`, each at its path from the workspace's root.
fn workspace(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a workspace");
    for (name, body) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().expect("a parent")).expect("its directories");
        fs::write(path, body).expect("the file");
    }
    dir
}

/// What `workspace` holds for an agent, read through the real interpreter.
async fn loaded(workspace: &tempfile::TempDir) -> Option<Instructions> {
    let interpreter = start_on_host().await;
    within(load(
        &interpreter,
        Some(workspace.path()),
        RepoInstructions::AgentsMd,
    ))
    .await
}

/// The file at the workspace's root is the one, and the one under `.agents/`
/// is not read when it is there.
#[tokio::test]
async fn a_file_at_the_root_is_the_one_and_the_other_is_not_read() {
    let workspace = workspace(&[
        ("AGENTS.md", b"Root rules.\n"),
        (".agents/AGENTS.md", b"Hidden rules.\n"),
    ]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join("AGENTS.md"));
    assert_eq!(found.file.text, "Root rules.\n");
    assert!(!found.section().contains("Hidden"), "{}", found.section());
}

#[tokio::test]
async fn the_file_under_dot_agents_is_found_when_the_root_has_none() {
    let workspace = workspace(&[(".agents/AGENTS.md", b"Hidden rules.\n")]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join(".agents/AGENTS.md"));
    assert_eq!(found.file.text, "Hidden rules.\n");
}

#[tokio::test]
async fn a_workspace_with_neither_file_has_no_instructions() {
    let workspace = workspace(&[("README.md", b"Not for agents.\n")]);
    assert_eq!(loaded(&workspace).await, None);
}

/// Nothing is read when the agent is set to leave the file out, or when the
/// session has no workspace to read it from, though the file is there.
#[tokio::test]
async fn nothing_is_loaded_when_it_is_switched_off_or_there_is_no_workspace() {
    let workspace = workspace(&[("AGENTS.md", b"Root rules.\n")]);
    let interpreter = start_on_host().await;
    let off = load(&interpreter, Some(workspace.path()), RepoInstructions::None);
    assert_eq!(within(off).await, None);
    let nowhere = load(&interpreter, None, RepoInstructions::AgentsMd);
    assert_eq!(within(nowhere).await, None);
}

/// A file with nothing in it tells an agent nothing, and the next place is
/// tried rather than the empty file shadowing it.
#[tokio::test]
async fn a_file_with_nothing_in_it_is_passed_over() {
    let workspace = workspace(&[
        ("AGENTS.md", b" \n\n"),
        (".agents/AGENTS.md", b"Hidden rules.\n"),
    ]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join(".agents/AGENTS.md"));
}

/// What the cap cut is not judged by its first bytes: a file that begins with
/// more blanks than the cap holds is not empty, and is still the file.
#[tokio::test]
async fn a_cut_file_that_begins_blank_is_still_the_file() {
    let body = format!("{}rules\n", " ".repeat(MAX_BYTES));
    let workspace = workspace(&[
        ("AGENTS.md", body.as_bytes()),
        (".agents/AGENTS.md", b"Hidden rules.\n"),
    ]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join("AGENTS.md"));
    assert_eq!(found.file.size, body.len());
    assert!(found.section().contains("[outrig:"), "{}", found.section());
}

/// Only a regular file is instructions: a directory of that name is not.
#[tokio::test]
async fn a_directory_of_that_name_is_not_the_file() {
    let workspace = workspace(&[
        ("AGENTS.md/notes.md", b"Not the file.\n"),
        (".agents/AGENTS.md", b"Hidden rules.\n"),
    ]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join(".agents/AGENTS.md"));
}

/// A repository that keeps one file under two names is read under the name
/// that was asked for, and the text is the target's.
#[tokio::test]
async fn a_symlink_is_followed_to_its_file() {
    let workspace = workspace(&[(".agents/AGENTS.md", b"Hidden rules.\n")]);
    symlink(".agents/AGENTS.md", workspace.path().join("AGENTS.md")).expect("a symlink");
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(found.path, workspace.path().join("AGENTS.md"));
    assert_eq!(found.file.text, "Hidden rules.\n");
}

/// A file that is not UTF-8 cannot be carried, and it does not hand its place
/// to the file after it: that would be a different file than the repository
/// named first.
#[tokio::test]
async fn a_file_that_is_not_utf8_loads_nothing_and_is_not_replaced_by_the_next() {
    let workspace = workspace(&[
        ("AGENTS.md", &[0xff, 0xfe, b'\n']),
        (".agents/AGENTS.md", b"Hidden rules.\n"),
    ]);
    assert_eq!(loaded(&workspace).await, None);
}

#[tokio::test]
async fn a_file_of_exactly_the_cap_is_carried_whole() {
    let body = "x".repeat(MAX_BYTES);
    let workspace = workspace(&[("AGENTS.md", body.as_bytes())]);
    let found = loaded(&workspace).await.expect("instructions");
    assert_eq!(
        (found.file.size, found.file.text.len()),
        (MAX_BYTES, MAX_BYTES)
    );
    assert!(
        !found.section().contains("[outrig:"),
        "a note on a whole file"
    );
}

/// The cap falls inside a character here, and the text stops short of it
/// rather than carrying half of one.
#[tokio::test]
async fn a_file_past_the_cap_is_cut_on_a_character_boundary_and_says_so() {
    let body = format!("a{}", "\u{e9}".repeat(MAX_BYTES));
    let workspace = workspace(&[("AGENTS.md", body.as_bytes())]);
    let found = loaded(&workspace).await.expect("instructions");

    assert_eq!(found.file.size, body.len());
    assert_eq!(found.file.text.len(), MAX_BYTES - 1);
    assert!(body.starts_with(&found.file.text));

    let path = workspace.path().join("AGENTS.md");
    let note = format!(
        "[outrig: `{}` is {} bytes; only the first {} are shown here. Read the rest from the \
         file when you need it.]",
        path.display(),
        body.len(),
        MAX_BYTES - 1
    );
    assert!(
        found.section().ends_with(&format!("\n\n{note}")),
        "{}",
        found.section()
    );
}

#[test]
fn the_section_names_the_file_then_gives_its_text() {
    let found = Instructions {
        path: PathBuf::from("/workspace/AGENTS.md"),
        file: FileText {
            size: 12,
            text: "Be careful.\n".to_string(),
        },
    };
    assert_eq!(
        found.section(),
        "The project's instructions for agents, from `/workspace/AGENTS.md`:\n\nBe careful."
    );
}
