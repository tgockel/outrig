//! Pins the discipline `crate::notice` depends on: in the modules that can run while the
//! REPL's prompt is up, nothing writes to the terminal except through `notice!`.
//!
//! A raw `eprintln!` there lands on the row the line editor believes it owns and garbles the
//! prompt (#439). `clippy::disallowed_macros` would say the same, but it is configured per
//! crate and the rest of the CLI -- setup progress, `outrig init`, `outrig build` -- is right
//! to print directly, so this scans the few modules instead.

use std::path::{Path, PathBuf};

/// Modules a background task, a watcher, or a subagent's round can write from while the user
/// is at the prompt. Directories are scanned recursively.
const GUARDED: &[&str] = &[
    "src/builtin_tool.rs",
    "src/cli/watcher.rs",
    "src/llm.rs",
    "src/llm",
    "src/subagent",
];

/// What a direct write to the terminal looks like.
const FORBIDDEN: &[&str] = &["eprintln!", "eprint!", "io::stderr", "dbg!"];

fn rust_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
            .map(|entry| entry.expect("directory entry").path())
            .collect();
        entries.sort();
        for entry in entries {
            rust_files(&entry, out);
        }
    } else if path.extension().is_some_and(|ext| ext == "rs") {
        out.push(path.to_path_buf());
    }
}

/// The part of a source file that ships: everything before the first top-level
/// `#[cfg(test)]`, which in these files always opens the test module.
fn shipped(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .take_while(|(_, line)| *line != "#[cfg(test)]")
        .map(|(index, line)| (index + 1, line))
}

#[test]
fn modules_that_can_overlap_a_prompt_write_through_notice() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for guarded in GUARDED {
        rust_files(&root.join(guarded), &mut files);
    }
    assert!(files.len() > 5, "found only {files:?}; did a module move?");

    let mut offenders = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).expect("read source");
        for (number, line) in shipped(&source) {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if let Some(found) = FORBIDDEN.iter().find(|pattern| code.contains(**pattern)) {
                offenders.push(format!(
                    "{}:{number}: `{found}`",
                    file.strip_prefix(root).unwrap_or(file).display()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "write through `crate::notice::notice!` instead, so a live prompt is not garbled:\n{}",
        offenders.join("\n")
    );
}
