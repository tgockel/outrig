//! The `podman exec` bootstrap fallback, forced with `OUTRIG_BOOTSTRAP=exec`.
//!
//! Its own test binary because the mode is read once per process: setting the
//! variable inside `runtime_user.rs` would decide the path for every test that
//! happens to run after it.
//!
//! ```sh
//! cargo test --features e2e runtime_user_fallback -- --nocapture
//! ```

#![cfg(feature = "e2e")]

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use common::{
    entry_for_id, init_tracing, install_shadow, pull_alpine, read_stdout, root_stdout, start_alpine,
};

#[tokio::test]
async fn forced_exec_fallback_still_bootstraps() {
    // Before anything reads the mode. Sole test in this binary, so no other
    // thread is running yet.
    unsafe { std::env::set_var("OUTRIG_BOOTSTRAP", "exec") };
    init_tracing();
    pull_alpine();

    let host_ws = tempfile::tempdir().expect("tempdir");
    let mut container = start_alpine(host_ws.path(), None).await;

    // The fallback runs `useradd`/`groupadd` inside the image, so this test --
    // unlike the direct path's -- has to put them there.
    install_shadow(container.name());

    container.bootstrap_user().await.expect("bootstrap_user");
    let user = container.user_name().expect("user name").to_string();

    // Same end state as the direct path: the host uid resolves to the name
    // the container reports, and an exec lands on it.
    let passwd = root_stdout(container.name(), &["cat", "/etc/passwd"]);
    assert_eq!(
        entry_for_id(&passwd, container.uid()).as_deref(),
        Some(user.as_str()),
        "/etc/passwd should resolve the host uid to {user}:\n{passwd}"
    );

    let mut child = container
        .exec_stdio(&["id".to_string()], &BTreeMap::new())
        .await
        .expect("exec_stdio id");
    let out = read_stdout(&mut child).await;
    assert!(
        out.contains(&format!("uid={}", container.uid())),
        "expected the host uid in `id` output, got: {out}"
    );

    container.stop(Duration::from_secs(2)).await.expect("stop");
}
