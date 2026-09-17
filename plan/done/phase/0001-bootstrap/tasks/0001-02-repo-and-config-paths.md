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

## Decisions

- **Don't go through `directories::ProjectDirs::from`.** The spec wording suggests it,
  but `ProjectDirs::config_dir()` on Linux falls back to `$HOME/.config/outrig` when
  `XDG_CONFIG_HOME` is unset, while `doc/reference/config.md:8` mandates
  `~/.outrig/config.toml` as the documented default. We read `XDG_CONFIG_HOME`
  ourselves and use `directories::BaseDirs::new()` only for `home_dir()`. The
  dedicated test `global_config_path_home_default_when_xdg_unset` pins this so a
  future "just use ProjectDirs" refactor breaks loudly.
- **`#[doc(hidden)] pub fn global_config_path_with`.** Pure resolver exposed for
  integration tests so they can inject `XDG_CONFIG_HOME` and `HOME` values without
  `unsafe { std::env::set_var(...) }` (unsynchronized in Rust 2024) or a
  `serial_test` dev-dep. Mirrors the `find_repo_root` / `find_repo_root_from`
  split the task notes already endorsed.
- **`dispatch(&Cli) -> Result<()>` in `main`.** Refactored `main` into
  parse / dispatch / report so the documented error path can fire from `Cmd::Run`
  via `?` while every other subcommand keeps a `NotImplemented(name)` stub.
  Centralizes the `eprintln!("error: {e}")` call in one place.
- **Override paths are taken verbatim.** `resolve_repo_config(Some(p), _)` does
  not check existence, canonicalize, or validate the filename. Validation is
  task 0005's territory; resolution stays pure.
- **`OutrigError::Io` formats as `"{0}"`, not `"io error: {0}"`.** `main` adds the
  `"error: "` prefix; doubling it (e.g. `"error: io error: ..."`) provides no extra
  context since `std::io::Error` already self-describes ("No such file or
  directory", "Permission denied", etc.).
- **Walk-up keeps going past empty `.agents/`.** A `.agents/` directory without
  an `outrig/config.toml` inside is not a stop signal; the walk continues to
  parents. `find_repo_root_skips_agents_dir_without_outrig_config` pins this
  invariant.
