# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0](https://github.com/tgockel/outrig/releases/tag/outrig-cli-v0.1.0) - 2026-06-26

### Added

- **Standalone image projects** -- `outrig image init` scaffolds a project whose build
  output is a reusable container image, `outrig image build` builds and verifies it, and
  `outrig image inspect` (local and `--remote`) reads its OCI labels without pulling
  layers or starting a container. Image config is stamped into labels rather than a baked
  file, and repo-local build images are named after their `[images.<name>]` config.
- **Design helper** -- `outrig design prompt --standalone` generates an AI-assisted prompt
  for standalone image projects.
- **Session management** -- `outrig clean` bulk-removes stopped session records with a
  30-day default retention window, preview/confirm, and `--older-than` / `--yes` controls.
- **Run ergonomics** -- a `run --model <NAME>` override, config-less `run`/`mcp` backed by
  the global config plus an explicit `--image`, and raw local image refs accepted for
  `--image`.
- **Resilient LLM calls** -- transient endpoint failures (request timeouts, dropped
  connections, HTTP 408/429/5xx) are retried with bounded exponential backoff, and the
  OpenAI provider's `request-timeout-secs` is now actually applied.
- Renamed the in-process model build feature from `mistralrs` to `local-llm`.

### Fixed

- Show the final image tag after a build.
- Render prompt help links as the published mdBook URLs while keeping local `doc/...`
  metadata for sync checks.
- Skip agent/model/provider validation during image builds.
- Repair e2e suite rot.

### Changed

- Split the workspace into the `outrig` library and the `outrig-cli` binary; each crate
  ships its own crates.io README.
- Renamed the `[container]` config table to `[image]` and the tool-call/result `cap`
  limits to `max`.
