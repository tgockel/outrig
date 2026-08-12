# `devices` documents a rejection it does not enforce

## Problem

`doc/reference/config.md` says of `[<...>.security].devices`:

> Entries are plain absolute paths; podman's `<src>:<dst>:<perms>` form is not accepted.

`validate_device_list` in `crates/outrig/src/config/validate.rs` does not check for `:`. An
entry like `/dev/fuse:/dev/fuse:rwm` is non-empty, is absolute, and is not a duplicate, so it
passes validation and reaches podman verbatim as `--device=/dev/fuse:/dev/fuse:rwm` -- which
podman accepts. The documented restriction is therefore not a restriction; the colon form
works, undocumented and untested.

Noticed while adding `unmask` (`0114`), whose `validate_unmask_list` *does* reject `:` --
because podman splits an unmask value on it, so a colon-joined entry would silently expand
back into several paths. That gives the two neighboring validators opposite treatments of the
same character, for reasons that are genuinely different but nowhere written down together.

## Goal

Make the code and the doc agree, in whichever direction is wanted. Two ways, and the choice is
a real one:

- **Enforce it.** Add a `DevicePathListSeparator` variant beside the existing `DevicePath*`
  ones. Consistent with `unmask`, and keeps a 1:1 relation between a config entry and an argv
  token. Risk: silently breaks any existing config relying on the undocumented behavior.
- **Document it instead.** Drop the sentence and support the colon form deliberately, with a
  validation rule for its shape. More surface, but `<src>:<dst>:<perms>` is how podman lets a
  caller remap a node or restrict it to read-only, which is a real thing to want.

Whichever wins, `validate_device_list` and `validate_unmask_list` should carry a comment
saying why they treat `:` differently, since they sit adjacent and otherwise read as copies.

## Acceptance

- No statement in `doc/reference/config.md` about `devices` entry shape is unenforced.
- A test covers the colon form for `devices`, asserting whichever behavior was chosen.
