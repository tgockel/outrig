# A relative `model-path` validates against one base and loads against another

## Context

`[models.<name>].model-path` is checked for existence against the repo root
(`crates/outrig/src/config/validate.rs`, `validate_mistralrs_model`):

```rust
let resolved = if path.is_absolute() {
    path.to_path_buf()
} else {
    root.join(path)
};
if !resolved.exists() {
    return Err(ConfigValidationError::MistralrsModelPathMissing { .. });
}
```

At runtime the same value is passed **verbatim**, never joined:
`crates/outrig-cli/src/llm.rs` hands `model.model_path.as_deref()` to
`crates/outrig-cli/src/llm/mistralrs.rs`'s `model_path: Option<&Path>`, which opens it directly.
So a relative `model-path` is validated against the repo root and then loaded against the
process's current directory. They agree only when `outrig` is invoked from the repo root, which
is the common case and is why this has not been noticed.

`crates/outrig/tests/fixtures/config-full.toml` uses a relative `model-path`
(`.agents/outrig/models/llama-3-8b-instruct.q4.gguf`), and `fixture_loads_end_to_end` creates it
under the repo root -- so the validation half is covered and the runtime half is not tested at
all.

`doc/reference/config.md` and `doc/concepts/in-process-llm.md` both say `model-path` "may be
absolute or relative to the repo root", which describes the validation and not the load.

## Goal

Make the path that is validated the path that is opened.

## Deliverables

- Resolve `model-path` once, against the same base validation used, and pass the resolved path
  to the mistralrs loader. The resolution belongs in the library beside the other config path
  helpers, not in the CLI.
- A regression test that runs from a directory that is *not* the repo root -- otherwise the bug
  is invisible. `crates/outrig-cli/tests/llm_resolve.rs` is the closest existing home.
- Whether the base is the repo root or the declaring file's directory is fork 1.

## Design forks

1. **Repo root vs. declaring file -- Recommended: declaring file.**
   `plan/done/0097-config-path-provenance.md` deliberately left `models` out of its provenance
   sweep ("Design fork 3 -- Open. Only images and mounts have paths today ... Start narrow"),
   which was right for that task but is exactly the situation fork 3 said to revisit once another
   entry grew a path. A global `[models.<n>]` with a relative `model-path` has the same problem a
   global `[images.<n>]` had, and the machinery now exists: stamp `Model` in
   `Config::stamp_source` and give it a `resolved_model_path(repo_root)` mirroring
   `MountConfig::resolved_host_path`. Confirm before committing: if no one declares models
   globally, repo-root-only is a smaller change and closes the validate/load split on its own.

2. **Whether to reject relative `model-path` instead -- Open.** A multi-gigabyte weights file is
   rarely inside the repo, so relative paths here may be a misfeature rather than a convenience.
   Rejecting them is the smallest possible fix and the most disruptive; the fixture would have to
   change, and it is the only in-tree example.

## Dependencies

None hard. Shares machinery with 0097, which has landed.

## See also

- `crates/outrig/src/config/validate.rs` -- `validate_mistralrs_model`, the validating half.
- `crates/outrig-cli/src/llm.rs`, `crates/outrig-cli/src/llm/mistralrs.rs` -- the loading half.
- `plan/done/0097-config-path-provenance.md` -- design fork 3, which deferred this deliberately.
- `crates/outrig/tests/fixtures/config-full.toml` -- the only relative `model-path` in tree.
