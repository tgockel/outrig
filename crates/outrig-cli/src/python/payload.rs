//! Finding the static CPython payload on the host.
//!
//! Nothing is downloaded during a build or a session; `scripts/fetch-python-payload.sh` puts
//! the interpreter in a cache directory once, and this module finds it. A missing payload is
//! reported precisely -- naming the architecture, the path searched, and the remedy -- rather
//! than silently falling back to an image-provided `python3`, which would be a different
//! interpreter with different modules and no guarantee of being there at all.

use std::path::PathBuf;

use crate::error::{OutrigError, Result};

/// Where the payload is bound inside the container.
pub const PAYLOAD_MOUNT: &str = "/outrig/python";

/// The interpreter, in the container's coordinates.
pub const CONTAINER_PYTHON: &str = "/outrig/python/bin/python3";

/// The extracted interpreter tree on the host, ready to bind-mount read-only.
///
/// The prototype is x86-64 only, and selects on the *host* architecture. Selecting on the
/// container image's architecture is what a real implementation has to do -- podman will run a
/// foreign-arch image under emulation without saying so.
pub fn payload_dir() -> Result<PathBuf> {
    let arch = std::env::consts::ARCH;
    let dir = cache_root().join("outrig/python").join(arch);
    if dir.join("bin/python3").is_file() {
        return Ok(dir);
    }
    Err(OutrigError::Configuration(format!(
        "no python payload for {arch}: expected an interpreter at {}\n\
         run scripts/fetch-python-payload.sh to download and verify it",
        dir.join("bin/python3").display(),
    ))
    .into())
}

fn cache_root() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from(".cache"))
}
