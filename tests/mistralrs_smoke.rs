#![cfg(feature = "mistralrs")]
//! End-to-end smoke for the in-process mistralrs shim. Each test is gated
//! behind an environment variable that points at a real GGUF model, since
//! loading any model takes seconds and ~hundreds of MB to gigabytes of disk.
//! Without those env vars the tests print a `skip:` notice and exit 0 so
//! `cargo test --features mistralrs` stays green in CI.

use std::path::Path;
use std::time::Duration;

use outrig::config::Config;
use outrig::llm::{LlmRegistry, RigAgent, build_agent, resolve_agent};
use rig::completion::Prompt;
use tempfile::TempDir;
use tokio::time::timeout;
use walkdir::WalkDir;

const TEST_MODEL: &str = "OUTRIG_MISTRALRS_TEST_MODEL";
const TEST_MODEL_ID: &str = "OUTRIG_MISTRALRS_TEST_MODEL_ID";
const TEST_MODEL_FILE: &str = "OUTRIG_MISTRALRS_TEST_MODEL_FILE";
const TEST_DEVICE: &str = "OUTRIG_MISTRALRS_TEST_DEVICE";

/// Generous wall-clock cap on the prompt round-trip. Anything slower than
/// this is almost certainly a hang -- the test environment chooses a model
/// small enough to fit comfortably.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);

fn optional_device_line() -> String {
    std::env::var(TEST_DEVICE)
        .ok()
        .map(|device| format!("device     = {device:?}\n"))
        .unwrap_or_default()
}

fn cfg_with_model_path(path: &str) -> String {
    let device = optional_device_line();
    format!(
        r#"
default-model = "local"

[providers.local]
style = "mistralrs"

[models.local]
provider   = "local"
model-path = "{}"
{device}

[agents.smoke]
preamble = "You are a terse assistant."
"#,
        path,
    )
}

fn cfg_with_model_id(id: &str, file: &str) -> String {
    let device = optional_device_line();
    format!(
        r#"
default-model = "local"

[providers.local]
style = "mistralrs"

[models.local]
provider   = "local"
model-id   = "{}"
model-file = "{}"
{device}

[agents.smoke]
preamble = "You are a terse assistant."
"#,
        id, file,
    )
}

async fn one_shot(agent: &RigAgent, prompt: &str) -> String {
    let RigAgent::Mistralrs { agent: inner, .. } = agent else {
        panic!("expected Mistralrs-backed agent");
    };
    timeout(PROMPT_TIMEOUT, inner.prompt(prompt).into_future())
        .await
        .expect("prompt did not complete within the timeout")
        .expect("prompt succeeded")
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_path_smoke() {
    let Ok(model_path) = std::env::var(TEST_MODEL) else {
        println!("skip: {TEST_MODEL} unset");
        return;
    };

    let cfg = Config::load_from_str(&cfg_with_model_path(&model_path)).expect("config parses");
    let resolved = resolve_agent(&cfg, "smoke").expect("agent resolves");

    let cache = TempDir::new().expect("tempdir");
    let registry = LlmRegistry::new();
    let agent = build_agent(&resolved, vec![], cache.path(), &registry)
        .await
        .expect("agent builds");

    let reply = one_shot(&agent, "Say hi.").await;
    assert!(
        !reply.trim().is_empty(),
        "expected a non-empty reply; got {reply:?}",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn download_path_smoke() {
    let Ok(model_id) = std::env::var(TEST_MODEL_ID) else {
        println!("skip: {TEST_MODEL_ID} unset");
        return;
    };
    let Ok(model_file) = std::env::var(TEST_MODEL_FILE) else {
        println!("skip: {TEST_MODEL_FILE} unset");
        return;
    };

    let cache = TempDir::new().expect("tempdir");
    let cfg =
        Config::load_from_str(&cfg_with_model_id(&model_id, &model_file)).expect("config parses");

    // First load: downloads.
    let resolved = resolve_agent(&cfg, "smoke").expect("agent resolves");
    let registry = LlmRegistry::new();
    let agent = build_agent(&resolved, vec![], cache.path(), &registry)
        .await
        .expect("first agent build (download path)");
    let first_reply = one_shot(&agent, "Say hi.").await;
    assert!(!first_reply.trim().is_empty());
    drop(agent);

    let blob_mtime_before = locate_gguf(cache.path(), &model_file)
        .map(file_mtime)
        .expect("downloaded GGUF should be discoverable in cache dir");

    // Second load: must not re-download.
    let agent = build_agent(&resolved, vec![], cache.path(), &registry)
        .await
        .expect("second agent build (cache reuse)");
    let _ = one_shot(&agent, "Say bye.").await;

    let blob_mtime_after = locate_gguf(cache.path(), &model_file)
        .map(file_mtime)
        .expect("cached GGUF still present after second load");

    assert_eq!(
        blob_mtime_before, blob_mtime_after,
        "second load re-downloaded the GGUF (mtime changed)",
    );
}

fn locate_gguf(cache_root: &Path, basename: &str) -> Option<std::path::PathBuf> {
    WalkDir::new(cache_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .find(|e| e.file_type().is_file() && e.path().file_name().is_some_and(|n| n == basename))
        .map(|e| e.into_path())
}

fn file_mtime(p: std::path::PathBuf) -> std::time::SystemTime {
    std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .unwrap_or_else(|e| panic!("metadata failed for {p:?}: {e}"))
}
