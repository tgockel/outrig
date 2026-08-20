# The e2e suite assumes it is the only outrig on the machine

`crates/outrig-cli/tests/mcp_sidecar_smoke.rs:489` failed once during the 0116
work, then passed three consecutive isolated runs and two full-suite runs with no
change to it. The assertion that failed was:

```
clean should report removing the stray: <outrig clean stderr>
```

The test plants a stopped container labeled `org.outrig.session=<sid>`, ages it
past the cutoff, runs `outrig clean -y --older-than 2s`, and requires the stray's
name in the report.

`outrig clean` sweeps by label across the **whole machine**, not per session: it
lists stopped, record-less containers carrying `org.outrig.session` and removes
them. So any other `outrig clean` running concurrently -- another test binary in
the same `cargo test` run, or a real session's teardown on the same host -- can
remove this test's stray first, after which the sweep under test has nothing to
report and the assertion fails. The failure surfaced while running the e2e suite
three times back to back with unrelated outrig sessions live on the machine,
which is exactly that interleaving.

Nothing about the planting or the sweeping goes through code 0116 changed: the
stray is created with a raw `podman run`, stopped with a raw `podman stop`, and
`crates/outrig-cli/src/cli/clean.rs` is untouched by that task.

Confirmed pre-existing rather than argued to be: with several unrelated outrig
sessions live on the machine, `trunk` at `a556ec4` fails this test the same way
in a full-suite run, from a scratch worktree, minutes apart from the branch that
was under review. In isolation it passes on both.

Options, roughly in order of preference: give the test its own label namespace
and have `clean` accept a filter so the sweep under test cannot see anyone else's
strays; or assert on the stray's *absence* afterwards rather than on its presence
in the report, which is the property that actually matters and is not
order-dependent; or serialize the sweeping tests behind a lock.

`clean_sweeps_...` is the clearest case because the mechanism is visible, but it
is not the only test that assumes exclusivity. During the same window
`entrypoint_stdio_audit_covers_first_packet` also failed once and then passed
three isolated runs and two full-suite runs; it drives the network interceptor
against a freshly initialized container while other sessions are starting and
stopping their own. Whatever is done for the sweep should be checked against the
timing-sensitive interceptor tests too.

Worth stating in `CONTRIBUTING.md` either way: the e2e suite currently expects an
idle machine, and a developer running it beside a live `outrig run` should not
read a single failure as a regression.

Note `plan/next/clean-batch-removal-fidelity.md` is adjacent -- it is about what
the batch removal reports, this is about what it is allowed to see.
