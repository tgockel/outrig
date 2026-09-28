# The GGUF picker selects a split quantization one shard at a time

## Context

`config_init::resolve_model_file` offers every GGUF a Hugging Face repo lists, sorted by path, and
`parse_pick_input` takes only 1-based indices and exact paths. Choosing a quantization split across
shards -- `*-00001-of-0000N.gguf` -- therefore means naming every shard: nine numbers for
`DeepSeek-R1-Q4_K_M/` in `unsloth/DeepSeek-R1-GGUF`. Miss one and the config still validates, the
rest downloads, and the load then fails: mistralrs refuses a set whose file count differs from its
`split.count`.

Every quantization in that repo is split, 3 to 30 shards each. Since #180 the picker's default is
the first file that is a whole model by itself, never a shard, so such a repo has no default and
no shortcut.

## Shape

- Let one token select a whole split set, or every GGUF under a directory.
- Or group the listing by quantization, one row per split set, so a single number picks all of its
  shards.

## Dependencies

Moot once `plan/next/remove-deprecated-local-llm.md` lands: the picker exists only for the
`mistralrs` style.

## See also

- `crates/outrig-cli/src/config_init.rs` -- `resolve_model_file`, `parse_pick_input`,
  `MODEL_FILE_PICK_FIELD`.
- `crates/outrig-cli/src/hf.rs` -- `is_split_shard`, which already recognizes a shard's name.
