# Application snapshots are never pruned

`0003-43` copies a session's application code whole to `~/.cache/outrig/applications/<digest>`
(`crates/outrig/src/harness/application.rs`, `snapshot_of`), and nothing removes one. The
bindings' requirement-set installs (`bindings/<hash>`) and the `outrig` package
(`package/outrig-<digest>`) keep every copy the same way, but those change with a requirement set
or a release; application code changes with every edit an owner makes between sessions, so a
cycle of edit and run leaves one snapshot per edit -- about 0.1 to 1 GB after a thousand cycles of
a 100 KB to 1 MB package.

A prune would remove snapshots no running session mounts and none touched recently, under the
lock `payload::locked_once` takes beside each, so a session copying or about to mount one never
loses it. `outrig clean` already removes what sessions leave behind (`doc/usage/sessions.md`,
"`outrig clean`") and is the natural place; a save (`0003-64`) that names a digest is a reason to
keep that one, or to say the code must be supplied again on resume, which it is anyway.
