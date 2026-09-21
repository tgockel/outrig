# Both `the_accept_loop_*` tests fail: no audit record is ever written

## Symptom

`cargo test -p outrig --lib` fails deterministically on two tests in
`crates/outrig/src/network.rs`:

```
failures:
    network::tests::the_accept_loop_keeps_taking_finished_connections_back
    network::tests::the_accept_loop_waits_for_the_connections_it_started

test result: FAILED. 400 passed; 2 failed; 0 ignored
```

Both spend 30 s and then report that the audit log is *empty*, not short:

```
panicked at crates/outrig/src/network.rs:5556:9:
the audit log never reached 1 records

panicked at crates/outrig/src/network.rs:6030:9:
assertion `left == right` failed: every connection the loop started owes a record
before it returns
  left: 0
 right: 4
```

`await_audit_records` polls for 30 s (`network.rs:5546`), which is where the runtime goes.
Not a flake: 4 for 4 with default threads, and 1 for 1 under `--test-threads=1` on an
otherwise idle machine. `SNIFF_TIMEOUT` is 750 ms, so the 30 s is the poll giving up rather
than a sniff window.

The shared fact is that **zero** records reach `NETWORK_LOG` -- one test wants 1, the other
wants 4, and both get 0. So this is not the accounting question in
`accept-loop-reaps-only-on-accept.md`; nothing is being written at all.

## Provenance

Both tests arrived with `a79bf881` ("fix: attach rolls back, and detach ends every bridge it
started", 0002-40), and `5dc5bd30` ("test: the reap test waits on records, not on the clock")
rewrote the first one's wait onto `await_audit_records`. Observed on `9df4482b`, on a branch
whose diff touches no file under `crates/*/src/`, so it is a property of trunk rather than of
the branch that found it.

## Open question, and why it is not resolved here

Found while landing 0002-48, whose diff cannot reach this code, so it was recorded rather
than chased. It was seen only inside a sandboxed shell, and both tests do two things a
sandbox can perturb: they bind and connect on loopback (`TcpListener::bind("127.0.0.1:0")`),
and `AuditSink::open` writes under `TMPDIR`. Neither is obviously implicated -- `NO_PROXY`
covers `127.0.0.1`, `tempfile::tempdir()` succeeds, and the connects themselves succeed --
but nobody has yet run these two on an unsandboxed host or checked a CI log for them.

**First step: confirm which it is.** Run the two on an ordinary shell, and check whether any
recent `cargo` matrix row on trunk is red on these names. If CI is green, the finding is that
the tests depend on ambient conditions they do not state; if CI is red too, it is the
`AuditSink` write path.

## See also

- `plan/next/accept-loop-reaps-only-on-accept.md` -- same loop, different claim (*when*
  handles are reaped, not whether records are written).
- `plan/next/lib-unit-test-flake.md` -- the other `--lib` failure, which is a genuine flake in
  `process_tests` and unrelated.
