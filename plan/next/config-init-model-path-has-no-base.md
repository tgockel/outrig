# `outrig init` writes a `model-path` against no stated base

> **On a deprecated surface.** `model-path` belongs to `style = "mistralrs"`, deprecated and
> scheduled for removal -- see `plan/next/remove-deprecated-local-llm.md`. Fold this in there
> rather than fixing it separately if the removal lands first.

## Context

0002-46 closed the *read* side of the `model-path` base question: a relative one resolves against
the repo root, in exactly one place (`Model::resolved_model_path`), for both the existence check
and the load. The *write* side has no rule at all.

`crates/outrig-cli/src/config_init.rs`'s `prompt_mistralrs_model` takes the answer and stores it
verbatim:

```rust
let path = ask_required(prompt, &MODEL_PATH_FIELD).await?;
(None, None, Some(PathBuf::from(path)), None)
```

and `MODEL_PATH_FIELD`'s description is `"Filesystem path to a GGUF file."` -- which names no
base. So a user running `outrig init` from a subdirectory and answering with a path relative to
*their* working directory gets a config that then fails validation, naming a path that exists.
The failure is the mirror image of the one 0002-46 fixed: read side centralized, write side
unanchored.

Found during 0002-46's `/simplify` pass. Pre-existing; that task did not introduce it and
deliberately did not widen into it.

## Deliverables

Either is defensible, and they are not exclusive:

- **Say the base.** `MODEL_PATH_FIELD.description` becomes something like `"Filesystem path to a
  GGUF file; relative paths are read from the repo root."` One line, no behavior change, and it
  matches how `doc/reference/config.md` now documents the key.
- **Absolutize at prompt time.** Join a relative answer to the repo root before storing it, so
  the written config says what the user meant. Needs a repo root in scope at
  `prompt_mistralrs_model`, which it does not have today, and writes an absolute path into a
  config that may be checked in -- which is why the first option is probably the right one alone.

## Acceptance

- A scripted `config_init` run answering `model-path` with a relative path produces a config that
  validates against the repo root it was generated for.
- `crates/outrig-cli/tests/config_init_scripted.rs` is the home; it already drives this prompt in
  `writes_mistralrs_config_with_model_id`.

## Dependencies

None. Cheap enough to fold into the removal entry's doc sweep if that lands first.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-46-deprecated-local-llm-behavior-for-0.2.0.md` -- the
  read side, and the decision that fixed the base at the repo root.
