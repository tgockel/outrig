# `resolve_mcp_env` hands back plaintext that `with_env` then shows

Tracked as #417, milestone 0.3.0.

## Symptom

None in outrig itself since #324: every path inside the workspace resolves with
`resolve_mcp_env_values` and hands the result to `with_resolved_env`, so a `${VAR}` value reaches
podman by name and every diagnostic shows `KEY=${VAR}`.

A library caller still has the old pair available. `outrig::resolve_mcp_env` returns
`BTreeMap<String, String>`, and the obvious next call, `ContainerCreateOptions::with_env` or
`ExecOptions::with_env`, takes exactly that. Every value then goes on podman's command line and
into every error, transcript line, and debug trace as written -- the leak #324 closed, reopened
one API call away. Nothing at either call says so beyond their docs.

## Why it waits for 0.3

Both signatures are public, and 0.2.x changes are additive only. 0.2.1 added the replacements
beside them rather than retyping them.

## Shape

Make the provenance-carrying form the only one that reads a `${VAR}`:

- `resolve_mcp_env` returns `BTreeMap<String, ResolvedEnvValue>` (absorbing
  `resolve_mcp_env_values`), or is removed in its favor.
- `with_env` keeps taking plain strings -- a caller-chosen literal is legitimately shown -- but
  its docs stop being the only thing standing between a resolved secret and the argv.

Deprecating `resolve_mcp_env` in a late 0.2.x release, pointing at `resolve_mcp_env_values`, is
the gentler first step if one ships before 0.3.
