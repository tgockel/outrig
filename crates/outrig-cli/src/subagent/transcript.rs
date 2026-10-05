//! Per-subagent transcript, written beside the MCP servers' stderr logs.
//!
//! A subagent's work is otherwise only visible as interleaved stderr traces,
//! which is fine live but useless afterwards. This gives each one a file named
//! `subagent-<name>.log`, matching the existing `logs/<server>.stderr`
//! convention so `outrig logs` lists it.
//!
//! Names are unique only among one launching agent's live subagents, so the
//! name alone cannot place a file. A subagent the primary launched writes
//! `<session_dir>/logs/subagent-<name>.log`; one launched by a subagent writes
//! into a `subagent-<parent>/` directory beside its parent's transcript, and so
//! on down. A name holds no `.`, so a directory never shares a path with a
//! file, and two subagents share one only when they share their whole path.
//!
//! Logging never fails a round: if the file cannot be opened or written, the
//! subagent keeps working and the transcript is simply absent.

use std::path::Path;

use tokio::fs::File;
use tokio::io::AsyncWriteExt;

use super::ModelLabel;

pub struct Transcript {
    file: Option<File>,
}

impl Transcript {
    /// Open the append log of the subagent `name`, launched by the subagents
    /// in `ancestry` -- outermost first, and empty when the primary launched
    /// it -- and write the header naming it.
    ///
    /// The header goes here rather than in a method of its own so it cannot be
    /// skipped or written out of order. The file is opened in append mode, so
    /// the header lands once per open rather than once per file: a name
    /// released and launched again appends to the same file, and its header is
    /// where the second subagent starts.
    pub async fn open(
        log_dir: &Path,
        ancestry: &[String],
        name: &str,
        label: Option<&ModelLabel>,
    ) -> Self {
        let mut dir = log_dir.to_path_buf();
        for parent in ancestry {
            dir.push(format!("subagent-{parent}"));
        }
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return Self { file: None };
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("subagent-{name}.log")))
            .await
            .ok();
        let mut this = Self { file };

        let path: Vec<&str> = ancestry.iter().map(String::as_str).chain([name]).collect();
        // The model detail only when the launch named one: an inherited launch
        // has nothing to confirm.
        let detail = label
            .map(|label| format!(" ({})", label.detail()))
            .unwrap_or_default();
        // The trailing newline keeps the next `=== prompt ===` block's leading
        // blank line, so the header does not run into it.
        this.write(&format!("=== subagent {}{detail} ===\n", path.join("/")))
            .await;
        this
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
        let mut log = Transcript::open(dir.path(), &[], "audit", None).await;
        log.record_prompt("check the config").await;
        log.record_reply("looking now").await;
        log.record_outcome("result", "two issues").await;

        let text = read_back(dir.path(), "audit").await;
        assert!(text.starts_with("=== subagent audit ===\n"), "got: {text}");
        assert!(text.contains("check the config"));
        assert!(text.contains("looking now"));
        assert!(text.contains("two issues"));
    }

    /// The parent's directory sits beside the parent's own transcript, and the
    /// header spells out the whole path the directories encode.
    #[tokio::test]
    async fn a_nested_transcript_lands_in_its_parents_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ancestry = ["parent-a".to_string(), "scan".to_string()];
        let mut log = Transcript::open(dir.path(), &ancestry, "audit", None).await;
        log.record_prompt("check the config").await;

        let nested = dir.path().join("subagent-parent-a").join("subagent-scan");
        let text = read_back(&nested, "audit").await;
        assert!(
            text.starts_with("=== subagent parent-a/scan/audit ===\n"),
            "got: {text}"
        );
        assert!(text.contains("check the config"));
    }

    /// The deepest tree the config allows, every name at the length limit,
    /// still gets its transcript: each ancestor adds a directory rather than
    /// lengthening one file name past the 255 bytes a path component may hold.
    #[tokio::test]
    async fn the_deepest_tree_still_gets_a_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = "a".repeat(crate::subagent::MAX_NAME_LEN);
        // The primary is depth 1, so subagents fill one layer fewer than the
        // ceiling, and every layer but the last is an ancestor.
        let layers = outrig::config::SUBAGENT_DEPTH_MAX_CEILING as usize - 1;
        let ancestry = vec![name.clone(); layers - 1];
        let mut log = Transcript::open(dir.path(), &ancestry, &name, None).await;
        log.record_prompt("deep").await;

        let mut nested = dir.path().to_path_buf();
        for parent in &ancestry {
            nested.push(format!("subagent-{parent}"));
        }
        assert!(read_back(&nested, &name).await.contains("deep"));
    }

    #[tokio::test]
    async fn rounds_append_rather_than_truncate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None).await;
        log.record_prompt("first").await;
        log.record_prompt("second").await;

        let text = read_back(dir.path(), "audit").await;
        assert!(text.contains("first") && text.contains("second"));
    }

    /// A subagent whose outcome came only from `set_result` has nothing to add
    /// here -- an empty `--- reply ---` block would just be noise.
    #[tokio::test]
    async fn an_empty_reply_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None).await;
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

        let mut log = Transcript::open(&blocker, &[], "audit", None).await;
        log.record_prompt("check the config").await;
        log.record_outcome("result", "found things").await;
    }
}
