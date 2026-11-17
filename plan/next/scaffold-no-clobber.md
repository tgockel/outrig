# Prevent concurrent scaffold overwrites

## Goal

Extend the no-clobber publication used for global config initialization (#793) to other
create-only scaffold writes.

## Deliverables

- `init/repo.rs::write_repo_config` must not replace a repo config created during its prompts.
- `image_setup/init.rs` must not replace Dockerfile/image.toml without `--force`, even if
  they appear after the early existence check. Keep an existing README even with `--force`,
  including one created between its check and write.
- Decide and document partial-write behavior for multi-file image scaffolding when a
  concurrent creation blocks publication; keep early checks for immediate conflicts.
- Add deterministic regression coverage and preserve forced rewrite permission/ACL behavior.

## Acceptance

Concurrent creators keep their contents for create-only writes. Forced Dockerfile/image.toml
writes still replace files, and README preservation does not turn a concurrent creation into
an overwrite.

## Dependencies

- Global config no-clobber writer fix (#793).
