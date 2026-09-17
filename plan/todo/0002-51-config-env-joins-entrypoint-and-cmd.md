# 0002-51 -- `Config.Env` joins `Config.Entrypoint` and `Config.Cmd` as a read

## Context

CocoClaw's `crates/cococlaw-agent/src/outrig/mod.rs` runs `podman image inspect --format
'{{json .Config.Env}}'` directly, in a private `image_env` helper (`mod.rs:1104-1109`) -- the one
container-runtime invocation it makes itself; every other one goes through OutRig. It exists
because a `view = "primary"` sidecar keeps its own image's environment, and a global sidecar row
cannot otherwise know a per-repository image's `PATH`.

OutRig already reads the same image config for two sibling fields. `read_image_entrypoint_cmd`
(`crates/outrig/src/image.rs:608-636`) shells `podman image inspect <tag> --format
'{{json .Config}}'` and parses `Entrypoint`/`Cmd`; `read_image_labels` (`image.rs:580-601`) does
the same for `Config.Labels`, returning a parsed `BTreeMap<String, String>`. `Config.Env` is the
same call, one field over -- the same parse, the same error handling, sitting one field away.

The only existing `Config.Env` read anywhere in this repo is an ad hoc one in a smoke test
(`crates/outrig-cli/tests/mcp_sidecar_smoke.rs:710-719`), and it inspects a *running container's*
env via `podman inspect`, not an image's -- a different target this task doesn't touch.

`plan/done/phase/0002-sidecars/tasks/0002-26-primary-view-relative-entrypoint.md` weighed and
rejected a host-side `Config.Env` read once before, but for a different job: resolving a relative
`ENTRYPOINT` against `PATH` needs the image's env *and* a filesystem probe per candidate, which
needs a container -- the thing that task's launcher exists to avoid. A plain accessor that only
returns the declared env, with no probing, doesn't hit that objection.

CocoClaw is the motivating consumer: it wants to stop shelling out to `podman` itself, and this is
the one call standing in the way. Its own follow-up spec sits in its own tree at
`plan/next/outrig-image-env-accessor.md`, blocked on this accessor existing in a released OutRig.
Nothing about the accessor itself is CocoClaw-specific -- any embedder that wants an image's
declared environment without starting a container has the same need.

## Goal

An embedder can read an image's `Config.Env` through OutRig, the same way it already reads
`Config.Entrypoint`, `Config.Cmd`, and `Config.Labels`, without shelling out to `podman` itself.

## Deliverables

- `pub async fn read_image_env(tag: &ImageTag, transcript: Option<&Transcript>) ->
  Result<BTreeMap<String, String>>` in `crates/outrig/src/image.rs`, beside
  `read_image_entrypoint_cmd`. Same invocation shape as its two siblings:
  `Cmd::new("podman").arg("image").arg("inspect").arg(tag.as_str()).arg("--format")
  .arg("{{json .Config.Env}}")`, then `process::run_capture_logged(cmd, "podman",
  transcript).await?`.
- Return type is a parsed `BTreeMap<String, String>`, matching `read_image_labels`'s precedent
  rather than `read_image_entrypoint_cmd`'s raw-vector one. A caller layering env by precedence
  (CocoClaw's row-env-wins forwarding) wants a map, not a `Vec<String>` it has to re-split itself.
- The `KEY=value` split and the null/empty handling live in a small private pure function (e.g.
  `fn parse_env_json(text: &str) -> Result<BTreeMap<String, String>>`), separate from the `podman`
  invocation -- the same separation `0002-26` made between candidate generation and the syscall loop
  around it. An entry with no `=` is skipped, not guessed at.
- Confirm empirically whether podman ever prints `Config.Env` as `null` (as it does for
  `Config.Labels`, which is why `read_image_labels` special-cases it at `image.rs:593-595`) or
  always emits an array; handle `null` the same way if it can happen.
- JSON-parse failures return `OutrigError::Configuration(format!("podman image inspect {tag}:
  invalid env JSON: {source}"))`, matching both siblings' error message shape exactly
  (`image.rs:596-600`, `627-631`).
- Doc comment matching the two siblings' voice and length.
- Insert `read_image_env` alphabetically into `crates/outrig/public-api.txt`'s `outrig::image`
  block, between the existing `read_image_entrypoint_cmd` line (922) and `read_image_labels` line
  (923), and regenerate the snapshot with the command `RELEASING.md` documents.

## Acceptance

- `parse_env_json` (or equivalent) has unit tests: a normal `KEY=value` set, an entry with no `=`
  (skipped, not guessed), and whichever `null`/empty-array behavior the implementation step found
  podman actually produces.
- No inline test for the `podman`-invoking outer function -- matching `read_image_labels` and
  `read_image_entrypoint_cmd`, neither of which has one. There's no `fake_podman` seam anywhere in
  the crate (only the `skopeo` remote path takes a program-name parameter for that), and adding
  one here alone would be scope beyond this task.
- `crates/outrig/public-api.txt` regenerated and clean.
- `cargo test`, `cargo clippy`, `cargo fmt --check` all pass.

## Dependencies

None hard.

## See also

- `plan/done/phase/0002-sidecars/tasks/0002-26-primary-view-relative-entrypoint.md` -- the related,
  distinct, prior decision about host-side `Config.Env` reads.
- CocoClaw's `plan/next/outrig-image-env-accessor.md` -- the consumer side. Not touched by this
  task and not a hard dependency, the same relationship `plan/done/0087` has to CocoClaw's own
  nested-podman entry.
