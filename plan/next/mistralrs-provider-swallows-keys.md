# `style = "mistralrs"` accepts any key at all

`LlmProvider` is an internally-tagged enum with `deny_unknown_fields`, which
rejects unknown keys correctly on the two remote variants:

```toml
[providers.p]
style          = "openai"
base-url       = "https://x"
api-key        = "${K}"
not-a-real-key = 1        # error: unknown field, expected one of `base-url`, ...
```

`Mistralrs` is a *unit* variant, and `deny_unknown_fields` has no fields to
check against, so the same typo is silently swallowed:

```toml
[providers.p]
style          = "mistralrs"
not-a-real-key = 1        # accepted, discarded
```

Found while writing the `request-timeout-secs` range check (0106). That task
rejects `request-timeout-secs = 0` on every remote provider, because reqwest
reads `Duration::ZERO` as an immediate timeout. The identical line under
`style = "mistralrs"` parses clean and validates clean -- not because `0` is
meaningful there, but because the key never reaches the parsed provider.

The blast radius is wider than one key. Every misspelling, every key copied from
a remote provider block, and every future key added to the remote variants is
accepted-and-discarded on a mistralrs provider. `deny_unknown_fields` is
load-bearing everywhere else in this schema -- `doc/reference/config.md` opens
by promising "Unknown keys are an error" -- so this is the one place the promise
does not hold.

## Sketch

Give the variant a body so serde has a field set to check against:

```rust
#[non_exhaustive]
Mistralrs {},
```

A braced-empty variant keeps the TOML spelling identical (`style = "mistralrs"`
with no other keys) while giving `deny_unknown_fields` something to reject
against. Worth checking whether serde's internally-tagged handling actually
enforces it for an empty field set, rather than assuming -- if it does not, the
fallback is a custom `Deserialize` for the enum, or a deny-list check beside
`reject_repo_network_policy`.

**This is a breaking change** to a `#[non_exhaustive]` enum variant: `Mistralrs`
is matched as a unit variant across both crates (`validate.rs`'s two exhaustive
matches, `llm.rs`, `config_init.rs`, and several tests), and a struct variant
needs `Mistralrs { .. }` at each. That is mechanical, but it is a source break
for downstream matchers too, so it wants the pre-0.2.0-final window rather than
a point release.

## Acceptance

- A `mistralrs` provider with an unknown key is a parse error naming the key.
- `style = "mistralrs"` alone still parses, and serializes byte-identically.
- `config_merge.rs`'s `mistralrs_provider_ignores_request_timeout_secs` flips
  from pinning today's swallow to asserting the rejection; it was written
  against the bug deliberately, and its doc comment points here.

## Dependencies

None. Sequence before 0.2.0 final if the variant reshape is wanted, since it is
a source break; the check itself is additive after that.
