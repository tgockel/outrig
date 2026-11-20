//! Podman query decoding must distinguish an empty store from a failed query.

mod common;

use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Output};

use common::parse_podman_names;

#[test]
fn successful_query_can_return_no_names() {
    let out = Output {
        status: ExitStatus::from_raw(0),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    assert!(parse_podman_names(&out).is_empty());
}

#[test]
fn successful_query_returns_names() {
    let out = Output {
        status: ExitStatus::from_raw(0),
        stdout: b"first\nsecond\n".to_vec(),
        stderr: Vec::new(),
    };
    assert_eq!(parse_podman_names(&out), ["first", "second"]);
}

#[test]
#[should_panic(expected = "podman ps failed (exit status: 125): store unreachable")]
fn failed_query_with_empty_stdout_reports_stderr() {
    let out = Output {
        status: ExitStatus::from_raw(125 << 8),
        stdout: Vec::new(),
        stderr: b"store unreachable".to_vec(),
    };
    parse_podman_names(&out);
}
