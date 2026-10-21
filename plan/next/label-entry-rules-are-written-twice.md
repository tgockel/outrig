# The label and `image.toml` entry rules are written twice

## Context

An `org.outrig.mcp` label and a standalone `image.toml` `[mcp]` table hold the same entries to
the same rules: a well-formed server name, not the reserved name, no `args`, a non-empty
command, no placement key, and since #338 a `call-timeout-secs` in `1..=3600`. The rules live
in two per-entry loops in `crates/outrig/src/container/embedded.rs`: `parse_mcp_table` reports
into `EmbeddedImageConfigError`, and `TryFrom<StandaloneImageTomlRaw>` reports into
`StandaloneImageTomlError`. Each enum carries a variant per rule, and the two sets are
identical, display strings included.

The loops were copies of each other before #338, and #338 added its rule to both. Every new
label rule costs two checks, two variants, and two tests, and nothing keeps the copies in
step: a rule added to one loop and not the other lets `outrig image build` stamp a label the
runtime then refuses, or the reverse.

## Sketch

One private `check_label_entry(server, spec) -> Result<(), LabelEntryRule>`, where
`LabelEntryRule` is a crate-private enum with one variant per rule, and a `From<LabelEntryRule>`
into each public enum. Both loops become one call each. The public variants stay as they are,
so the change is internal and fits a patch release.

The `image.toml` read also refuses an empty table (`McpEmpty`) before the per-entry rules.
That is a rule about the table rather than an entry, and stays where it is.

Once the rules have one home, `mcp_call_timeout_secs_in_range` in `config/validate.rs` has a
single caller outside `check_mcp_call_timeout_secs` and could fold into it.
