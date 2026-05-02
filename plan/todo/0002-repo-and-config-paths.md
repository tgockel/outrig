# 0002 -- Repo and config paths

## Goal

Locate the repo config and the global config -- before parsing them. Subsequent tasks can rely
on these helpers.

## Deliverables

- `src/repo.rs::find_repo_root() -> Result<PathBuf>` walks up from cwd looking for
  `.agents/outrig/config.toml`. On miss: returns the exact error from
  `doc/usage/run.md`'s "When something goes wrong" section
  (`error: no .agents/outrig/config.toml found in current directory or any parent`).
- `src/repo.rs::repo_config_path(root: &Path) -> PathBuf` -- returns
  `<root>/.agents/outrig/config.toml`.
- `src/repo.rs::global_config_path(override: Option<&Path>) -> PathBuf` -- resolution order:
  `override` flag value > `$XDG_CONFIG_HOME/outrig/config.toml` (via
  `directories::ProjectDirs::from`) > `~/.outrig/config.toml`.
- Wire `--config` and `--global-config` global flags into the clap `Cli` struct from 0001.
- `tests/repo_paths.rs` covering:
  - No `.agents/outrig/` anywhere up the tree -> well-formed error.
  - Nested cwd finds the parent's config.
  - cwd at the file's dir finds it.
  - Explicit `--config` overrides the walk-up.
  - `XDG_CONFIG_HOME` env var sets global config location.
  - `--global-config` flag wins over `XDG_CONFIG_HOME`.

## Acceptance

- `cargo test repo_paths` passes.
- From a directory without a repo config, `outrig run` prints the documented error and exits 1.
- From a directory inside a repo (with config nested under `.agents/outrig/`), the helper
  resolves to the right path.

## Dependencies

- 0001-cargo-skeleton

## Notes

- Use `tempfile::tempdir` for tests; create the `.agents/outrig/config.toml` fixture inside
  and `chdir` (or pass an explicit cwd argument to `find_repo_root` to keep it pure -- prefer
  the latter; `find_repo_root_from(cwd: &Path)` makes tests cleaner).
- The walk-up should stop at the filesystem root, not at `$HOME` or anywhere else.
