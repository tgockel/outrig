# Pin the book toolchain in the standard image

## Problem

`.agents/outrig/images/outrig-standard/Dockerfile` installs the book toolchain unpinned:

```dockerfile
RUN cargo install mdbook mdbook-mermaid lychee --locked
```

`--locked` pins each tool's *dependencies*, not the tools' own versions, so every rebuild
resolves whatever is current. The preprocessor and the renderer then drift apart: `mdbook build`
currently warns that `mdbook-mermaid` was built against mdBook 0.5.0 while the installed mdBook
is 0.5.4. The build still succeeds, so this is a warning today rather than a failure -- but the
version pair is exactly the kind that eventually stops being compatible, and the image gives no
signal about which half moved.

CI installs the same way (`.github/workflows/ci.yml`, the mdbook job), so a break would land in
both places at once, on a day nothing in the repo changed.

## Sketch

Pin both, and let the pin be the thing that gets bumped deliberately:

```dockerfile
RUN cargo install mdbook@0.5.4 mdbook-mermaid@<matching> lychee@<current> --locked
```

Worth deciding at the same time whether CI and the image should share one source for those
versions rather than repeating them -- a small script, or a `[workspace.metadata]` block the
Dockerfile and workflow both read.

This is the same class as the unpinned `npm install`/`pip install` layer 0104 removed from this
image, and the reason is the same: a rebuild months from now should produce the image the
Dockerfile describes.

## See also

- `plan/done/0104-dogfood-sidecar-mcp-config.md` -- pinned the MCP sidecar images for this reason,
  including an SDK pin that was load-bearing within a day of being written.
