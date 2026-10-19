# `run_program`'s stderr assertions fail under the developer's `RUST_LOG`

## Symptom

With `RUST_LOG=debug` in the shell that runs `cargo test`, two tests in
`crates/outrig-cli/tests/run_program.rs` fail:

- `the_requirements_example_runs_on_the_host_alone`
- `a_workspace_path_that_is_not_utf8_is_refused_and_none_still_runs`

Both assert the CLI's stderr exactly: empty in the first, and one error line in the second. The
CLI they spawn inherits `RUST_LOG`, so its stderr also carries
`DEBUG outrig_cli::cli::app: outrig starting`. With `RUST_LOG` unset, both pass, and CI sets
none. Found while running the workspace's tests for `0003-28`, which does not touch either path.

## Shape

The harness that spawns the CLI (`crates/outrig-cli/tests/common/`) removes `RUST_LOG` from the
child's environment, as the interpreter harnesses clear `PYTHONUSERBASE` for the same reason: a
test asserts what the program writes, not what the developer's shell asks it to.
