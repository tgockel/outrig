//! A raw image ref that is not in podman's local store is refused with an
//! error that names where the ref was written and the `podman pull` that
//! fixes it (#355). Run with the stubbed runtime of [`common::run_outrig`], so
//! podman reports every image missing and nothing reaches a registry.
//!
//! The sidecar spellings need a real primary container before a sidecar's
//! image is ensured, so they are asserted in `mcp_sidecar_smoke.rs`.

mod common;

use std::fs;

use common::{ABSENT_IMAGE, GLOBAL_WITH_MODEL, run_outrig, utf8};

#[tokio::test]
async fn a_missing_image_flag_ref_names_the_flag_and_the_pull() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    fs::create_dir_all(&proj).unwrap();
    let global = tmp.path().join("global.toml");
    fs::write(&global, GLOBAL_WITH_MODEL).unwrap();
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&sessions).unwrap();

    let (ok, stderr) = run_outrig(
        &proj,
        &[
            "--global-config",
            utf8(&global),
            "--session-root",
            utf8(&sessions),
            "run",
            "--image",
            ABSENT_IMAGE,
        ],
    )
    .await;

    assert!(!ok, "a ref podman does not have must not start:\n{stderr}");
    assert!(
        stderr.contains(&format!(
            "image {ABSENT_IMAGE:?} (from --image) is not an [images.<name>]"
        )),
        "the error names the ref and the flag it came from:\n{stderr}",
    );
    assert!(
        stderr.contains(&format!("`podman pull {ABSENT_IMAGE}`")),
        "the error names the command that fixes it:\n{stderr}",
    );
}
