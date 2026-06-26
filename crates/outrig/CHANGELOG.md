# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/tgockel/outrig/releases/tag/outrig-v0.1.0) - 2026-06-26

### Added

- **Standalone toolset images** -- build a reusable OutRig image from a project's
  `image.toml`: `outrig image build` builds the declared Dockerfile and verifies the
  result, embedded `image.toml` is validated, and the MCP config is stamped into OCI
  labels (replacing the baked `/etc/outrig/image.toml` file). Built images are tagged
  after their `[images.<name>]` config, and repo-local build images carry the same
  `org.outrig.mcp` declaration that startup reads.
- **Image introspection** -- `outrig image inspect <ref>` reads OCI labels from the local
  store, and `outrig image inspect --remote <ref>` reads them from a registry via
  `skopeo` -- both without pulling layers or starting a container.
- **Embedded-MCP policy** -- `LaunchSpec` carries an `EmbeddedMcpPolicy`: `Merge` (the
  backward-compatible default) overlays the launch spec onto the image's embedded MCP
  map, while `Ignore` treats the provided map as authoritative and skips
  `org.outrig.mcp`.
- **Flexible launch inputs** -- `run`/`mcp` work without a repository config (falling back
  to the global config plus an explicit `--image`), an `--image` that matches no config
  block is used as a raw local Podman ref, and `outrig run` accepts a `--model` override.

### Fixed

- Skip agent/model/provider validation during image builds -- building an image never
  instantiates a model, so a dangling `default-model` no longer blocks it.
- Repair e2e suite rot.

### Changed

- Split the workspace into the `outrig` library and the `outrig-cli` binary, narrowed the
  runtime crate surface, and hid internal runtime modules.
- Centralized dependency versions in the workspace and declared features per crate; each
  crate now ships its own crates.io README.
- Renamed the `[container]` config table to `[image]` and the tool-call/result `cap`
  limits to `max`.
