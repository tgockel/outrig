//! The `outrig-enter` launcher: bytes and placement.
//!
//! `outrig-enter` is a statically linked musl helper that runs as a sidecar's
//! ENTRYPOINT, joins the primary container's mount namespace, grafts the
//! sidecar's own rootfs aside, and execs the real payload -- giving a
//! third-party MCP image the *primary's* filesystem view. Its source is
//! [`launcher.rs`] (+ the pure [`elf`] parser); the crate `build.rs` compiles
//! it for `<arch>-unknown-linux-musl` and drops the result in `OUT_DIR`, which
//! is embedded below.
//!
//! This module owns the bytes and the write; task 0090 owns the read-only
//! bind-mount that makes [`materialize`]'s output the sidecar's entrypoint.
//!
//! When the build machine lacks the musl target the embedded artifact is empty
//! and [`is_available`] is false -- [`materialize`] then fails with an
//! actionable message rather than writing a broken binary.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::{OutrigError, Result};

// The pure ELF parser is unit-tested on the host; `launcher.rs` pulls the same
// file in with `include!` for the musl build. It has no non-test consumer in
// the library, so it is compiled only under `cfg(test)`.
#[cfg(test)]
mod elf;

/// The launcher binary, statically linked for this build's architecture.
/// Empty when the crate was built without the musl target (see module docs).
const OUTRIG_ENTER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/outrig-enter"));

/// Whether this build embedded a usable launcher. False means the crate was
/// compiled without the `<arch>-unknown-linux-musl` target installed.
pub fn is_available() -> bool {
    !OUTRIG_ENTER.is_empty()
}

/// Write the launcher into `session_dir` as `outrig-enter`, mode `0755`, and
/// return its path. Task 0090 bind-mounts it read-only as the sidecar's
/// entrypoint. Fails when the launcher was not built into this binary.
pub fn materialize(session_dir: &Path) -> Result<PathBuf> {
    if !is_available() {
        return Err(OutrigError::FilesystemHelperUnavailable);
    }
    let path = session_dir.join("outrig-enter");
    std::fs::write(&path, OUTRIG_ENTER)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}
