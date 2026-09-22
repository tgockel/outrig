//! Shelling out to the container engines, for [`super::clean`]'s sweeps.
//!
//! Each sweep asks an engine for a JSON listing and then hands it a batch of
//! things to remove, and each wants the same two failures reported the same
//! way: an engine that is not installed, and one that refused. Kept together
//! so the answers cannot drift apart per sweep.

use std::process::Stdio;

use crate::error::{OutrigError, Result};

/// Run `program args...` and return its stdout.
///
/// A missing engine is reported as a sentence naming what wanted it, rather
/// than as a bare `NotFound`: the sweeps differ in what they require -- the
/// default one needs only podman, and `--build-containers` adds buildah --
/// so which binary is missing is only half of what the reader needs.
pub async fn capture(program: &'static str, args: &[&str], wanted_by: &str) -> Result<Vec<u8>> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                OutrigError::Configuration(format!("`{wanted_by}` needs `{program}` on PATH"))
            } else {
                OutrigError::Configuration(format!("running {program} {}: {source}", args.join(" ")))
            }
        })?;
    if !output.status.success() {
        return Err(OutrigError::Configuration(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(output.stdout)
}

/// Remove every target in one invocation, propagating failure.
///
/// `clean` should report something it could not remove rather than claiming
/// success, so this does not swallow a non-zero exit. Callers guard the empty
/// case: both engines reject a removal with nothing to remove.
pub async fn remove_batch(
    program: &'static str,
    verb: &[&str],
    targets: Vec<String>,
) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(verb)
        .args(&targets)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(OutrigError::Configuration(format!(
            "{program} {} {} failed: {}",
            verb.join(" "),
            targets.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(())
}
