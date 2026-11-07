# e2e coverage for the record-removal race (#869)

`clean` and `discard` now re-check liveness immediately before each record removal (unit-tested
in `crates/outrig-cli/tests/session_cli.rs`), closing the race where a container started while
the `[y/N]` prompt was open. What is still missing is the podman-gated reproduction the original
report ran by hand: a scratch session root and a throwaway `--rm` container, `clean --session`
(and separately `discard`) held at its prompt behind a fifo, the container started with
`podman start` while the command waits, and an assertion that the record, `logs/`, and
`outrig-enter` survive and the command names the session as kept rather than removed.

No podman is available in the environment this task was done in, so the test could not be
written and verified against a real engine with any confidence. Add it as an `e2e`-gated test
(`--features outrig/e2e,outrig-cli/e2e`), following the fifo-synchronized prompt pattern already
used in `crates/outrig-cli/tests/primary_view_e2e.rs` and `run_smoke.rs`, next to the reproduction
in GitHub issue #869.
