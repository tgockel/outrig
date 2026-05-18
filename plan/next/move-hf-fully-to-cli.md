# Consider moving `hf` module entirely into `outrig-cli`

## Goal

The library currently keeps `hf::{HfFile, HfTreeFetcher}` (trait + struct)
so `init` could swap a fake fetcher in tests. After the split, `init`
lives in `outrig-cli`, so the trait can move there too. Library users have
no reason to know about HuggingFace.

## Open question

Does any library-exposed API surface the `HfTreeFetcher` trait, or does
`config` reference it? If not, move the whole `hf.rs` into the bin crate
and drop the module from `outrig`.

## Acceptance

- `crates/outrig/` has no `hf` module and no references to it.
- `crates/outrig-cli/src/hf.rs` carries the trait, the real impl, and the
  test fakes.
