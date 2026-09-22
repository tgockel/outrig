# The interceptor trusts nft's exit code for what it installed

## Context

0002-53 found that `nft_rules` emitted `create table inet <t> { chain output { ... } }`, and
that nft 1.0.9 parses that, exits **0**, creates the table, and silently drops the nested block.
The interceptor then redirected nothing: `audit` recorded no connection, `filter` allowed every
one, and both `attach` and `detach` reported success. The script is flat now, so that particular
script cannot fail that particular way again.

What has not changed is that nothing checks what the engine actually committed. `attach` decides
the install worked from `nft -f`'s exit status, and the only structural check anywhere is the e2e
suite's `netns_has_outrig_table` (`crates/outrig/tests/network_interceptor.rs:147`), which greps
`nft list tables` for the name. That passed throughout the outage: the table *did* exist. Nine
behavioral tests were the only thing that noticed, and they had never been run.

## Why it might matter

This is the failure mode a security control can least afford: enforcement silently absent while
every status says it is present. The class is wider than the one syntax that caused it -- a
future nft that rejects a rule it used to accept, a rule that parses but does not install, a
`--echo` format change -- and each of them lands in the same place.

## Goal

An `attach` that cannot show the engine committed the ruleset it asked for fails, and undoes
itself, rather than returning a handle to an interceptor that enforces nothing.

## Deliverables

- **A check on the commit echo.** `install_interception` (`crates/outrig/src/network.rs`) already
  runs `nft --echo --handle -f` and parses the result with `table_handle_in`, so the evidence is
  in hand and unparsed. Measured against nft 1.0.9, the working form echoes one `# handle N` per
  committed object and the broken form echoed only the table line -- so counting handles against
  what `nft_rules` declared (1 table + 1 chain + 4 rules) discriminates, and does it without
  matching on nft's rule syntax, which normalizes.
- **Failure that tears down.** `attach` already runs `rollback.undo_now` on an `Err` from
  `install_interception`, and by this point the undo is narrowed to the handle, so a rejected
  install removes exactly what it made.
- **The fake's echo has to change with it.** The unit-test fake's default nft echo is
  `create table inet <t> # handle <n>` and nothing else -- a byte-for-byte reproduction of the
  broken engine's output, which roughly eight attach tests currently assert success against.
  That fixture was modeling the defect; it needs to echo a full commit, and that is most of the
  work in this entry.

## Design forks

1. **Count versus shape -- Recommended: count.** A count is version-tolerant and catches the
   whole class. Matching on `chain output` or on rule text pins outrig to one nft's rendering.
2. **Whether to re-read instead of reading the echo -- Recommended: no.** A separate
   `nft list table` after the commit is a second transaction, and what it returns can have been
   replaced between the two; the echo is the committing transaction's own word, which is the
   same argument `install_interception` already makes for taking the handle from it.

## Dependencies

- None. Independent of `plan/next/network-interceptor-mitm.md`.
