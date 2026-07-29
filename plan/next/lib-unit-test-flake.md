# Rare flake in `cargo test -p outrig --lib`

## Symptom

`process::process_tests::try_capture_logged_traces_spawn_and_exit_at_debug` fails
intermittently, in one of two complementary ways -- either assertion can be the one that
trips, and whichever fails reports that the *other* event is the only one present:

```
panicked at crates/outrig/src/process_tests.rs:251:5:
debug output should record exit code and elapsed time, got:
  DEBUG outrig::process: spawn command=/bin/echo hi
```

```
panicked at crates/outrig/src/process_tests.rs:247:5:
debug output should name the full command line, got:
  DEBUG outrig::process: exit program="/bin/echo" code=Some(0) elapsed_ms=0
```

So the test's own command runs and its writer is wired correctly; exactly one of the two
`tracing::debug!` events in `try_capture_logged` goes missing.

## Reproduction

Reproduces at roughly 1 run in 8 with the default thread count, on the compiled binary
directly (no cargo, no build churn):

```sh
for i in $(seq 1 40); do
  ./target/debug/deps/outrig-<hash> 2>&1 | grep -q FAILED && echo "failed on $i"
done
```

(Invoking the built binary directly also sidesteps the unrelated `$HOME` defect in
`primary-view-payload-home.md`, which makes `cargo clippy` fail outright inside a
`view = "primary"` shell sidecar.)

Measured rates:

| Selection                                                     | Failures |
|---------------------------------------------------------------|----------|
| Full lib suite, default threads                               | ~1 in 8  |
| Full lib suite, `--test-threads=1`                            | 0 / 10   |
| Suite minus `logged_capture_tees_command_and_output_to_transcript` | 0 / 25   |
| Only that test plus the failing one                            | 2 / 25   |

The third and fourth rows are the finding: removing one specific *other* test makes it
disappear, and that one test alone is enough to bring it back.

## Cause

Cross-test interference through tracing's global callsite state, not a timing race in the
code under test.

`logged_capture_tees_command_and_output_to_transcript` calls `run_capture_logged`, which
delegates to `try_capture_logged` (`process.rs:264`) -- so it executes the *same two
`tracing::debug!` callsites* as the failing test. It installs no subscriber. The failing
test installs a `DEBUG` subscriber, but via `tracing::subscriber::set_default`, which is
**thread-local**, while `tracing` caches per-callsite interest **globally**.

When the two run on different threads, the subscriber-less test can have a callsite
register or re-evaluate interest while no subscriber is visible on its thread, and the
cached "not interested" answer is then honored on the thread that does have one. That the
two events are cached independently is what produces the two complementary failure
modes -- whichever callsite loses the race is the one missing from the capture.

This supersedes the earlier suspect list (`network.rs` ephemeral port, `mcp.rs` 250 ms
timeout, `image.rs` sleep / `SystemTime::now`); none of those is involved. It also
explains why the original sighting resisted 54 repeats: the trigger is thread interleaving
within one binary, so it is invisible to `--test-threads=1` and unrelated to the
concurrent `cargo build` that happened to be running.

## Suggested fix

The test asserts on a global side channel from a thread-local subscriber, which is not
sound however the assertions are written. Options, cheapest first:

- Have the failing test use `tracing::subscriber::with_default` and drive the work on the
  *same* thread, and give it a private callsite by asserting through a dedicated wrapper
  rather than the shared `try_capture_logged` -- removes the sharing entirely.
- Serialize the tracing-observing tests against every other test that touches those
  callsites, with a shared `Mutex` (or the `serial_test` crate). Cheap, but it encodes an
  ordering constraint that a future caller of `try_capture_logged` can silently violate.
- Move both tracing-assertion tests into their own integration test binary, so they get a
  process with no competing callers. Most robust; `relocate-unit-shaped-tests.md` is
  adjacent work.

Prefer the first: it fixes the unsoundness rather than hiding it, and needs no new
dependency.

## See also

- `plan/next/relocate-unit-shaped-tests.md` -- the same tests are candidates to move.
- `plan/next/test-helper-consolidation.md` -- other `init_tracing` / subscriber duplication.
