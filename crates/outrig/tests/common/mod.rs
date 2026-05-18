//! Shared test helpers across library integration tests. Cargo treats
//! files in `tests/` as test binaries; subdirectories with `mod.rs` are
//! conventional shared modules (no phantom `common` test binary).

/// Install a best-effort tracing subscriber for integration tests that
/// surface process output under `--nocapture`.
#[allow(dead_code)]
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}
