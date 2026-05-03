# 0028 -- Publish docs to GitHub Pages via CI

## Goal

Publish the mdbook documentation in `doc/` to GitHub Pages on every push to `trunk`, so the
design-first docs are always reachable at a stable URL. This unblocks linking to the docs
from the README, issues, and external write-ups.

## Deliverables

- `.github/workflows/docs.yml` -- GitHub Actions workflow with two jobs:
  - `build`: checkout, install `mdbook` + `mdbook-mermaid` (prefer the prebuilt release
    tarball over `cargo install` -- saves ~1 minute), run `mdbook-mermaid install` then
    `mdbook build`, upload `target/book/` via `actions/upload-pages-artifact@v3`.
  - `deploy`: depends on `build`; publishes via `actions/deploy-pages@v4`. Uses the
    `github-pages` deploy environment so the run shows up in the repo's deployments view.
- Workflow triggers:
  - `push` to `trunk` filtered to paths under `doc/`, `book.toml`, and
    `.github/workflows/docs.yml`.
  - `workflow_dispatch` so deploys can be re-run manually without an empty commit.
- Workflow-level permissions: `contents: read`, `pages: write`, `id-token: write`.
- Concurrency group `pages` with `cancel-in-progress: false` (per the GitHub Pages docs --
  prevents overlapping deploys from clobbering each other).
- Pin the `mdbook` and `mdbook-mermaid` versions in the workflow (either via env vars at
  the top of the file or by extending whatever pinning lives in `book.toml` / repo tooling).
- README touch-up: link to the published URL (`https://<owner>.github.io/outrig/`) once the
  first deploy is live.

## Acceptance

- A push to `trunk` that touches `doc/` triggers the workflow; the resulting Pages site
  renders `doc/SUMMARY.md`'s TOC on the left and any mermaid diagrams inline.
- Re-pushing a doc edit re-deploys within a few minutes.
- The published URL is linked from the top-level README.
- The workflow takes well under 5 minutes end-to-end on the GitHub-hosted runner (target
  ~60-90s once binaries are warm).

## Dependencies

- None. Independent of the runtime work; can land alongside any in-flight Phase F/G task.

## Notes

- One-time repo setup that has to happen *outside* this PR: in the repo settings, set
  Pages -> Source -> "GitHub Actions". Note this in the PR description so reviewers don't
  forget. The first workflow run will fail until that flip happens.
- `mdbook-mermaid install` writes `theme/` and `mermaid.min.js` into the book's source
  tree. Decide whether to commit those (simpler, but adds vendored JS to the repo) or run
  `install` fresh in CI on every build (slightly slower, but keeps the source tree clean).
  Default suggestion: run fresh in CI.
- If a docs-only push is ever made on a branch other than `trunk` and you want a preview
  deploy, that's a follow-up -- v0 only deploys from `trunk`.
- The build job can stay on `ubuntu-latest`; no podman/buildah needed since this is a
  pure mdbook compile.
