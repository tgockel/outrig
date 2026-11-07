# A relative --session-root bypasses the live-owner check

Flagged in review of the #869 fix (pre-existing, unrelated to that change).

A relative `--session-root` is persisted into a session's record as a relative
`session_dir`. Cleaning the same store from a different working directory makes
`owner_is_live` probe that stale relative path -- resolved against the *current* cwd,
not the one the session was created from -- while `SessionStore::remove_by_id` resolves
the real entry under the (correctly re-resolved) root. During a cold image build, no
container exists yet to compensate, so a short `--older-than` can delete an active
launch's record and the files it owns despite its owner lock still being held, just not
at the path being probed.

Fix: probe the entry-resolved directory (the one `remove_by_id` would act on) rather than
the persisted `session_dir` field, in both `clean` and `discard`'s pre-removal checks.

Location: `crates/outrig-cli/src/cli/clean.rs` (around the `owner_is_live` calls added for
#869, but the underlying path mismatch predates that change).
