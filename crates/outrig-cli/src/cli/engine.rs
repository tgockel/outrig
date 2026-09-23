//! Shelling out to the container engines, for [`super::clean`]'s sweeps.
//!
//! Each sweep asks an engine for a JSON listing and then hands it a batch of
//! things to remove, and each wants the same two failures reported the same
//! way: an engine that is not installed, and one that refused. Kept together
//! so the answers cannot drift apart per sweep.

use std::process::Stdio;
use std::time::Duration;

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

/// `podman rm -f` every container carrying `label`, reporting failure.
///
/// A selector rather than a name, for the one reason that matters at reap
/// time: podman frees a name the instant its container exits, so a removal
/// that resolves a name a moment later can reach whatever took it. A label the
/// session minted cannot be taken by anything else.
///
/// Matching nothing is success. The container may already be gone -- `--rm`,
/// or an orderly stop that got there first -- and podman exits zero for a
/// filtered removal that selects nothing.
pub async fn remove_by_label(program: &'static str, label: &str, budget: Duration) -> Result<()> {
    // Not `remove_batch`, though the argv is the same shape. That one is
    // `outrig clean`'s, awaited without a deadline because the sweep is what
    // the user is waiting for and has nothing else to get on with. This one
    // runs on a path that must not be held up by it, so it is bounded -- and
    // the two differ in the part that matters, not just in wording.
    //
    // `kill_on_drop`, so neither ending leaves a child behind: the timeout
    // below drops this future, and so does aborting the task that awaits it.
    // A bound alone would turn a wedged `podman rm` into an orphan instead of
    // a hang, which is not an improvement.
    let run = tokio::process::Command::new(program)
        .args(["rm", "-f", "--filter"])
        .arg(format!("label={label}"))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(budget, run).await {
        Ok(output) => output?,
        Err(_) => {
            return Err(OutrigError::Configuration(format!(
                "{program} rm -f --filter label={label} did not answer within {budget:?}"
            ))
            .into());
        }
    };
    if !output.status.success() {
        return Err(OutrigError::Configuration(format!(
            "{program} rm -f --filter label={label} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An "engine" that ignores its argv and never answers -- the shape of a
    /// wedged `podman rm`, which a real `podman` binary cannot be asked to
    /// reproduce on demand.
    fn wedged_engine() -> &'static str {
        let dir = std::env::temp_dir().join(format!("outrig-wedged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("wedged-engine");
        std::fs::write(&path, "#!/bin/sh\nexec sleep 60\n").expect("write fake engine");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake engine");
        }
        // Leaked because `remove_by_label` takes a `&'static str` program, as
        // every caller passes a literal. One path per test run.
        Box::leak(path.to_string_lossy().into_owned().into_boxed_str())
    }

    /// A wedged engine must not hold its caller forever.
    ///
    /// This is the defect the session watcher's reap could reach: it awaited
    /// every removal before signalling that the primary had died, so one
    /// `podman rm` that never answered was a session that never ended.
    #[tokio::test]
    async fn a_label_removal_that_never_answers_is_given_up_on() {
        let budget = Duration::from_millis(300);
        let started = std::time::Instant::now();
        let outcome = remove_by_label(wedged_engine(), "org.outrig.instance=x", budget).await;
        let waited = started.elapsed();

        assert!(
            outcome.is_err(),
            "a removal that did not answer has removed nothing, and must say so"
        );
        assert!(
            waited >= budget,
            "it has to actually wait out the budget, or this proves nothing: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(30),
            "the budget has to bound the wait, not merely be declared: {waited:?}"
        );
    }
}
