# The GGUF picker accepts two independent quantizations at once

## Context

`config_init::parse_pick_input` concatenates the paths of every row picked. Picking two complete
split sets (say `Q4_K_M` and `Q8_0`) yields all their shards in one `model-file`. Config
validation accepts it, the loader downloads both sets, and then mistralrs rejects the list
because the reader count differs from `split.count`. A user could always name the same paths by
hand, so this predates the row grouping.

## Shape

- Refuse, at the pick, a combination of rows that are independent quantizations: more than one
  split set, or a split set plus a whole-model file.
- Keep repeated aliases of one row (a number and one of its shard paths) working.

## See also

- `crates/outrig-cli/src/config_init.rs` -- `parse_pick_input`.
- `crates/outrig/src/config/validate.rs` -- where `model-file` is validated.
