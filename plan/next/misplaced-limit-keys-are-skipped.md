# A limit key in the wrong table is skipped with a warning

## Symptom

The lenient loader drops a key a table does not take, with a warning, so a file written for a
newer `outrig` still loads (`config/lenient.rs`). That serves a key from the future; it is the
wrong rule for a key this build knows, written in the wrong table. `0003-50` refuses
`model-request-limit` and `model-request-warnings` on a `[models.<name>]` row (`misplaced`),
since dropped there they leave every agent unlimited. The same hazard stands for
`token-budget`, `children-max`, `requests-max`, `model-concurrency-max` and
`subagent-depth-max` on a model row, and for any of them under `[providers.<name>]`: each is
skipped, with a warning the only sign. Found by `0003-50`'s altitude review.

## Shape

One list of the limit keys -- `validate.rs`'s `Limited` fields, or the schema's top-level
limits -- and a rule beside `is_strict` that never skips one wherever it appears outside the
top level and `[agents.<name>]`, the error naming where the key belongs. It changes what loads
for the keys already shipped, so the changelog says so.
