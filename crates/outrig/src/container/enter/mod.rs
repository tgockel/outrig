//! The `outrig-enter` launcher: bytes and placement.
//!
//! `outrig-enter` is a statically linked helper that runs as a sidecar's
//! ENTRYPOINT, joins the primary container's mount namespace, grafts the
//! sidecar's own rootfs aside, and execs the real payload -- giving a
//! third-party MCP image the *primary's* filesystem view. Its source is
//! `launcher.rs` (+ the pure `elf.rs` parser); the crate `build.rs` compiles
//! it for `<arch>-unknown-linux-musl` -- or, without that target, statically
//! for the build machine's own target -- and drops the result in `OUT_DIR`,
//! which is embedded below.
//!
//! This module owns the bytes and the write; task 0002-13 owns the read-only
//! bind-mount that makes [`materialize`]'s output the sidecar's entrypoint.
//!
//! When neither build works the embedded artifact is empty and
//! [`is_available`] is false -- [`materialize`] then fails with an actionable
//! message rather than writing a broken binary.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::error::{IoPathExt, OutrigError, Result};

// The ELF parser, the `PATH` expansion and the symlink resolution and the library closure are
// unit-tested on the host; `launcher.rs` pulls the same files in with
// `include!` when `build.rs` compiles it, and `build.rs` includes `elf.rs`
// itself. Canon, PATH expansion and the library closure have no non-test
// consumer in the library and compile only under `cfg(test)`. The ELF parser
// also checks the Python payload at build time and locates its stack patch
// when unpacking, so it is always compiled; what of it only the launcher and
// the library closure use is dead code outside tests.
#[cfg(test)]
mod canon;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod elf;
// `libs.rs` borrows the other two files' items, which under `include!` share
// one scope; here they are siblings, so the imports are made around it.
#[cfg(test)]
mod libs {
    use super::canon::*;
    use super::elf::*;

    include!("libs.rs");
}
#[cfg(test)]
mod path_search;

/// The launcher binary, statically linked for this build's architecture.
/// Empty when neither the musl build nor its fallback worked (see module
/// docs).
const OUTRIG_ENTER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/outrig-enter"));

/// Whether this build embedded a usable launcher. False means the crate was
/// compiled without the `<arch>-unknown-linux-musl` target installed and the
/// static build for the build machine's own target failed too.
pub fn is_available() -> bool {
    !OUTRIG_ENTER.is_empty()
}

/// Write the launcher into `session_dir` as `outrig-enter`, mode `0755`, and
/// return its path. Task 0002-13 bind-mounts it read-only as the sidecar's
/// entrypoint. Fails when the launcher was not built into this binary.
pub fn materialize(session_dir: &Path) -> Result<PathBuf> {
    if !is_available() {
        return Err(OutrigError::FilesystemHelperUnavailable);
    }
    let path = session_dir.join("outrig-enter");
    std::fs::write(&path, OUTRIG_ENTER).path_ctx("write", &path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .path_ctx("set permissions on", &path)?;
    Ok(path)
}
