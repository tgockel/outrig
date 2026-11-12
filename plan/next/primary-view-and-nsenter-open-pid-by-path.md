# The primary-view bind mount and `nsenter` still name the init by PID

#630 pinned the init process (`ProcPin`) and made bootstrap, the interceptor's socket binding, its
resolver, and its namespace identity read through the pin. Two uses can only take a number:

- `view = "primary"` sidecars bind `/proc/<pid>/ns` (`ContainerCreateOptions`, `outrig_.rs`,
  `session_setup.rs`), which podman resolves by path.
- The interceptor's `nsenter -t <pid> -U -n nft ...` commands (`nsenter_net`). The apply and the
  by-handle undo check `/proc/self/ns/net` inside the entered process against the namespace read
  through the pin (#912), so they refuse rather than act elsewhere; the name-based undo armed
  across the apply (`delete_nft_table`) still trusts the number, guarded only by
  `Rollback::same_namespace` beforehand and the per-attach table name.

`Container::pid()` checks the pin is alive immediately before returning, so the window is narrow,
but it is not closed. Look at `nsenter --net=/proc/self/fd/N` with an inherited descriptor (see the
`namespace_id` notes on why the undo path avoids one) and at a pidfd-backed bind source.
