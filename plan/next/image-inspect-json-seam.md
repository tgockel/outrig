# One `podman image inspect` seam behind the three `Config.X` readers

## Context

`crates/outrig/src/image.rs` now has three accessors that shell the same command for three
fields of the same OCI config object:

| Function                     | Format                     | Returns                     |
|------------------------------|----------------------------|-----------------------------|
| `read_image_labels`          | `{{json .Config.Labels}}`  | `BTreeMap<String, String>`  |
| `read_image_entrypoint_cmd`  | `{{json .Config}}`         | `(Vec<String>, Vec<String>)`|
| `read_image_env`             | `{{json .Config.Env}}`     | `BTreeMap<String, String>`  |

Each builds the same six-arg `Cmd`, runs it through `process::run_capture_logged`, and lossily
decodes stdout. Two of the three then repeat the same JSON framing: trim, treat `""` and bare
`null` as absent, else `serde_json::from_str` with a `podman image inspect {tag}: invalid
<what> JSON: {source}` `Configuration` error. That is ~8 lines duplicated 30 lines apart, and
-- the part that actually bites -- two independent copies of the `null`-sentinel decision and
of the error-message format, free to drift.

`read_image_env` (task `0002-51`) is what made this a pattern rather than a pair. That task
deliberately did not fix it: consolidating means refactoring two already-shipped functions
immediately before the 0.2.0-rc.3 cut, for a benefit no current caller collects.

## Shape

A crate-private seam, leaving all three public signatures byte-identical so
`crates/outrig/public-api.txt` does not move:

```rust
async fn inspect_json<T: DeserializeOwned + Default>(
    tag: &ImageTag,
    format: &str,
    what: &str,          // "labels" / "env" / "config", for the error message
    transcript: Option<&Transcript>,
) -> Result<T>
```

Each accessor becomes one line plus its own post-processing (the `KEY=value` fold for env, the
`unwrap_or_default` pair for entrypoint/cmd). `read_image_entrypoint_cmd`'s function-local
`Config` struct already has all-`Option` fields, so `Default` derives.

## Why it is not done yet

- **No caller needs two fields of one image.** Verified at the time of writing: every
  `read_image_labels` call site (`container/embedded.rs:192,305,332`, `image.rs`,
  `outrig-cli/src/image_setup/inspect.rs:33`) wants labels alone; both
  `read_image_entrypoint_cmd` call sites (`outrig_.rs:1114`,
  `outrig-cli/src/cli/session_setup.rs:1239`) want entrypoint/cmd alone. The one-invocation
  win has no beneficiary in this tree.
- **The three do not share a null policy today.** `read_image_labels` and `read_image_env`
  treat empty stdout and `null` as an empty value; `read_image_entrypoint_cmd` has no such
  branch and hard-errors on a `null` config. A shared helper has to pick one, which is a
  behavior change to a released function -- the reason this wants its own task rather than a
  drive-by.

## Trigger

Do this when either lands: a fourth `.Config.X` accessor appears (`WorkingDir`, `User`, and
`Volumes` are the plausible ones), or a caller genuinely needs two fields off one image -- an
out-of-tree `view = "primary"` consumer wanting entrypoint/cmd *and* env for the same tag is
the nearest candidate. Three near-identical readers is the rule-of-three boundary, not past it.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-51-config-env-joins-entrypoint-and-cmd.md` -- the
  task that added the third reader and deferred this.
