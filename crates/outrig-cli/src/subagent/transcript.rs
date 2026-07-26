//! Per-subagent transcript, written beside the MCP servers' stderr logs.
//!
//! A subagent's work is otherwise only visible as interleaved stderr traces,
//! which is fine live but useless afterwards. This gives each one a file at
//! `<session_dir>/logs/subagent-<name>.log`, matching the existing
//! `logs/<server>.stderr` convention so `outrig logs` can reach it.
//!
//! Logging never fails a round: if the file cannot be opened or written, the
//! subagent keeps working and the transcript is simply absent.

use std::path::Path;

use tokio::fs::File;
use tokio::io::AsyncWriteExt;

pub struct Transcript {
    file: Option<File>,
}

impl Transcript {
    pub async fn open(log_dir: &Path, name: &str) -> Self {
        if tokio::fs::create_dir_all(log_dir).await.is_err() {
            return Self { file: None };
        }
        let path = log_dir.join(format!("subagent-{name}.log"));
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .ok();
        Self { file }
    }

    pub async fn record_prompt(&mut self, prompt: &str) {
        self.write(&format!("\n=== prompt ===\n{prompt}\n")).await;
    }

    pub async fn record_reply(&mut self, reply: &str) {
        if reply.is_empty() {
            return;
        }
        self.write(&format!("\n--- reply ---\n{reply}\n")).await;
    }

    pub async fn record_outcome(&mut self, kind: &str, body: &str) {
        self.write(&format!("\n--- {kind} ---\n{body}\n")).await;
    }

    async fn write(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(text.as_bytes()).await.is_err() {
            // Stop trying after the first failure rather than erroring once
            // per line for the rest of the session.
            self.file = None;
            return;
        }
        let _ = file.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_back(dir: &Path, name: &str) -> String {
        tokio::fs::read_to_string(dir.join(format!("subagent-{name}.log")))
            .await
            .expect("transcript exists")
    }

    #[tokio::test]
    async fn records_the_round_under_a_name_matching_the_log_convention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), "audit").await;
        log.record_prompt("check the config").await;
        log.record_reply("looking now").await;
        log.record_outcome("result", "two issues").await;

        let text = read_back(dir.path(), "audit").await;
        assert!(text.contains("check the config"));
        assert!(text.contains("looking now"));
        assert!(text.contains("two issues"));
    }

    #[tokio::test]
    async fn rounds_append_rather_than_truncate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), "audit").await;
        log.record_prompt("first").await;
        log.record_prompt("second").await;

        let text = read_back(dir.path(), "audit").await;
        assert!(text.contains("first") && text.contains("second"));
    }

    /// The streaming path returns an empty reply for the primary agent, and a
    /// subagent whose outcome came only from `set_result` has nothing to add
    /// here -- an empty `--- reply ---` block would just be noise.
    #[tokio::test]
    async fn an_empty_reply_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), "audit").await;
        log.record_reply("").await;

        assert!(
            !read_back(dir.path(), "audit").await.contains("reply"),
            "an empty reply should not open a block"
        );
    }

    /// Logging is best-effort: a subagent whose transcript cannot be opened
    /// still runs. Creating the dir under a *file* makes `create_dir_all` fail.
    #[tokio::test]
    async fn an_unusable_log_dir_degrades_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        tokio::fs::write(&blocker, b"x").await.expect("write file");

        let mut log = Transcript::open(&blocker, "audit").await;
        log.record_prompt("check the config").await;
        log.record_outcome("result", "found things").await;
    }
}
