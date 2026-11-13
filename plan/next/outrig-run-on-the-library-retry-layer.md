# `outrig run` keeps its own copy of the retry layer

`crates/outrig-cli/src/llm/retry.rs` and `crates/outrig/src/agent/retry.rs` are
two implementations of one idea: an HTTP client that retries transient failures
under a shared budget, and a model wrapper that retries a `200` rig could make
no turn of. They were written apart, and they drift.

The drift is not theoretical. #721 taught the library's layer to name the
provider's finish reason for a reply it cannot use; #742 had to teach the CLI's
copy the same thing a release later, and #743 then found that copy reading two
of the three dialects -- the Responses `status` and `incomplete_details.reason`
having been added to the library's reader when this line gained
`style = "responses"`, and not to the CLI's. Each fix was small; there have been
three of them for one behavior.

What the library's layer has that the CLI's does not, as of this writing:

- usage read from a body rig rejected (`body_usage`), so a refused turn is still
  billed;
- the retries it records as events (`Retries` here, `Settling` in
  `crates/outrig/src/agent/ledger.rs`), which the CLI has no event log for but a
  future one would want;
- a settled body kept per attempt rather than one slot per client.

## Sketch

`outrig run` builds its chain in `crates/outrig-cli/src/llm.rs`
(`build_candidate`, `link`). The library's layer is `pub(crate)` today, so the
move means deciding what of it becomes public API -- the policy, the two
wrappers -- and taking the `public-api.txt` snapshot that goes with it. The
CLI's failover chain and its `notice!` lines are its own and would stay: the
library reports through events, the CLI through the terminal, so the seam is
whether the library's layer can be given a reporter.

This is #742's second correction, the one only the 0.3 line can make. It is a
refactor with no user-visible change of its own, which is why it has not been
done under any of the three issues above.

## Acceptance

- `outrig run` retries transient HTTP failures and unusable replies through the
  library's retry layer, and `crates/outrig-cli/src/llm/retry.rs` holds only what
  the terminal needs.
- The retry line and the final error read as they do today, held by the tests
  that cover them now.
- `crates/outrig/public-api.txt` is updated for whatever the move publishes.
