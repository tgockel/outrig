# Rare flake in `cargo test -p outrig --lib`

## Symptom

Observed once while verifying 0092:

```
running 180 tests
test result: FAILED. 179 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
error: test failed, to rerun pass `-p outrig --lib`
```

The failing test's name was filtered out of the captured output, so it is not yet known.

## What is known

- **Not caused by 0092.** `-p outrig --lib` compiles only `crates/outrig/src/`; 0092 touched
  only `crates/*/tests/*.rs` and `.github/workflows/ci.yml`. The flake predates the branch.
- **Rare.** Not reproduced in ~54 subsequent runs: 6 full `cargo test`, 5 `cargo test -p outrig
  --lib`, 3 more under full CPU saturation (20 cores pegged), and 40 direct invocations of the
  compiled test binary.
- **Circumstantial timing.** The one failure landed on the first `cargo test` after building
  with `--features outrig/e2e,outrig-cli/e2e`, i.e. while cargo was recompiling the default
  feature set. That suggests sensitivity to concurrent build load or to shared filesystem
  state under `target/`, rather than to CPU contention alone (which did not reproduce it).

## Suspects

Timing- or environment-sensitive spots in `crates/outrig/src/`, none yet confirmed:

- `network.rs:987` -- binds a real `std::net::TcpListener` on port 0. Ephemeral-port
  allocation is the classic source of this failure shape.
- `mcp.rs:389` -- `tokio::time::timeout(Duration::from_millis(250), child.wait())`; a 250 ms
  budget is thin on a loaded machine.
- `image.rs:1163` -- a 1 ms `tokio::time::sleep` inside a retry loop.
- `image.rs:686` -- `SystemTime::now()`, wall-clock dependent.

## Suggested approach

Run the lib test binary in a loop with a concurrent `cargo build` churning `target/` (the
condition under which it actually appeared), rather than with pure CPU load. Capture the full
output on failure -- the immediate need is the test's name. Once named, decide between fixing
the race and marking it `#[ignore]` with a tracking note.

Consider `cargo test -- --nocapture --test-threads=1` on repro to rule out cross-test
interference through shared state.
