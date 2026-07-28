// Pure `PATH` expansion for the launcher's payload program.
//
// A sidecar image is free to declare `ENTRYPOINT ["node", "/app/dist/index.js"]`
// -- `docker.io/mcp/filesystem` does -- so the launcher has to do for a bare
// program name what `execvp` would: walk `PATH` and take the first entry that
// opens. The search runs before the setns, against the launcher's own
// environment, which is the sidecar image's: the program lives in the sidecar's
// rootfs, so that is the right `PATH` and the wrong one is the target's.
//
// No syscalls and no I/O -- the caller does the opening. This file is the single
// source of truth: `launcher.rs` pulls it in with `include!`, so its header must
// be plain `//` comments (inner `//!` docs are illegal mid-file) and its `use`
// declarations must not collide with that file's. The `outrig` crate also
// compiles it as `#[cfg(test)] mod path_search` to run these tests on the host.

use std::ffi::OsStr;
use std::path::PathBuf;

/// The `PATH` to search when the environment carries none: what podman gives a
/// container whose image declares no `PATH`. Reaching for it keeps a payload
/// runnable in the one case where the environment cannot say where to look;
/// which list was walked is in the failure message either way.
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The `PATH` `program` is searched along, or `None` when it names a path and is
/// used literally.
///
/// The two callers -- the search itself and the message a failed search prints
/// -- go through this one function, so the value reported is always the value
/// walked.
fn searched_path<'a>(program: &OsStr, path_var: Option<&'a OsStr>) -> Option<&'a OsStr> {
    // A name with a separator in it is a path, not something to look up. That
    // is `execvp`'s rule, and it is the launcher's pre-existing behavior for an
    // absolute `ENTRYPOINT`.
    if program.as_encoded_bytes().contains(&b'/') {
        return None;
    }
    match path_var {
        Some(p) if !p.is_empty() => Some(p),
        _ => Some(OsStr::new(DEFAULT_PATH)),
    }
}

/// The paths to try opening for `program`, in order.
///
/// Unlike `execvp`, an empty or relative `PATH` entry contributes nothing: the
/// launcher hands a dynamic payload's path to the loader with the graft point
/// prefixed (`/mnt` + path), and a path that is not absolute cannot be
/// relocated that way. Such an entry means "the current directory" and no image
/// puts its entrypoint there; skipping it keeps every candidate graftable.
fn program_candidates(program: &OsStr, path_var: Option<&OsStr>) -> Vec<PathBuf> {
    let Some(path) = searched_path(program, path_var) else {
        return vec![PathBuf::from(program)];
    };
    std::env::split_paths(path)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(program: &str, path: Option<&str>) -> Vec<String> {
        program_candidates(OsStr::new(program), path.map(OsStr::new))
            .into_iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_program_with_a_slash_is_used_literally() {
        assert_eq!(
            candidates("/usr/local/bin/node", Some("/bin:/usr/bin")),
            ["/usr/local/bin/node"],
        );
        // Relative but slash-bearing: still literal, exactly as `execvp` has it.
        assert_eq!(candidates("./server", Some("/bin")), ["./server"]);
        assert_eq!(searched_path(OsStr::new("/bin/node"), None), None);
    }

    #[test]
    fn a_bare_name_expands_over_path_in_order() {
        assert_eq!(
            candidates("node", Some("/usr/local/bin:/usr/bin:/bin")),
            ["/usr/local/bin/node", "/usr/bin/node", "/bin/node"],
        );
        assert_eq!(candidates("node", Some("/")), ["/node"]);
    }

    #[test]
    fn empty_and_relative_entries_contribute_nothing() {
        // "" and "." both mean the current directory, and "bin" is relative to
        // it; none of the three can be graft-prefixed for the loader.
        assert_eq!(
            candidates("node", Some(":/usr/bin:.:bin:")),
            ["/usr/bin/node"],
        );
    }

    #[test]
    fn an_absent_or_empty_path_falls_back_to_the_default() {
        let expected = [
            "/usr/local/sbin/node",
            "/usr/local/bin/node",
            "/usr/sbin/node",
            "/usr/bin/node",
            "/sbin/node",
            "/bin/node",
        ];
        assert_eq!(candidates("node", None), expected);
        assert_eq!(candidates("node", Some("")), expected);
        // And the failure message names that list rather than an empty one.
        assert_eq!(
            searched_path(OsStr::new("node"), None),
            Some(OsStr::new(DEFAULT_PATH)),
        );
    }
}
