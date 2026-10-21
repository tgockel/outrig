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
//!
//! Every file operation is a plain `std::fs` call rather than a `tokio::fs`
//! one (#307). Tokio copies the bytes into a job on its blocking pool and
//! returns, and only the flush after it waits for the job, so a subagent
//! aborted in that flush leaves its write behind, and a name released and
//! launched again could put its header in the file before the write landed. A
//! synchronous write has no await point for an abort to land in, so a record
//! that was issued is in the file by the time `release` returns: the CLI runs
//! on current-thread runtimes, where `release` cannot overlap a poll of the
//! task it aborts. The price is a small local append on the runtime's thread
//! for each record.

use std::fs::File;
use std::io::Write as _;
use std::path::Path;

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
    /// where the second subagent starts, below everything the first wrote.
    pub fn open(
        log_dir: &Path,
        ancestry: &[String],
        name: &str,
        label: Option<&ModelLabel>,
    ) -> Self {
        let mut dir = log_dir.to_path_buf();
        for parent in ancestry {
            dir.push(format!("subagent-{parent}"));
        }
        if std::fs::create_dir_all(&dir).is_err() {
            return Self { file: None };
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("subagent-{name}.log")))
            .ok();
        let mut this = Self { file };

        // The model detail only when the launch named one: an inherited launch
        // has nothing to confirm.
        let detail = label
            .map(|label| format!(" ({})", label.detail()))
            .unwrap_or_default();
        // The trailing newline keeps the next `=== prompt ===` block's leading
        // blank line, so the header does not run into it.
        this.write(&format!(
            "=== subagent {}{detail} ===\n",
            super::subagent_path(ancestry, name)
        ));
        this
    }

    pub fn record_prompt(&mut self, prompt: &str) {
        self.write(&format!("\n=== prompt ===\n{prompt}\n"));
    }

    /// Which models answered the round's model calls, when a chain moved off its
    /// head (#265). `answered` holds indices into `chain`, the launch's
    /// candidates in chain order; one line per candidate, in the header's own
    /// shape.
    ///
    /// Nothing when the head alone answered, which is every round of a
    /// single-candidate launch: the header already names where the subagent
    /// started, so a subagent that never moved keeps the transcript it had. An
    /// index `chain` has no label for is skipped, for the reason a failed write
    /// is -- logging never fails a round.
    pub fn record_answered(&mut self, chain: &[ModelLabel], answered: &[usize]) {
        if answered.iter().all(|&index| index == 0) {
            return;
        }
        let lines: Vec<String> = answered
            .iter()
            .filter_map(|&index| chain.get(index))
            .map(ModelLabel::detail)
            .collect();
        self.record_outcome("answered by", &lines.join("\n"));
    }

    pub fn record_reply(&mut self, reply: &str) {
        if reply.is_empty() {
            return;
        }
        self.write(&format!("\n--- reply ---\n{reply}\n"));
    }

    pub fn record_outcome(&mut self, kind: &str, body: &str) {
        self.write(&format!("\n--- {kind} ---\n{body}\n"));
    }

    fn write(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if file.write_all(text.as_bytes()).is_err() {
            // Stop trying after the first failure rather than erroring once
            // per line for the rest of the session.
            self.file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_back(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(format!("subagent-{name}.log")))
            .expect("transcript exists")
    }

    #[test]
    fn records_the_round_under_a_name_matching_the_log_convention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None);
        log.record_prompt("check the config");
        log.record_reply("looking now");
        log.record_outcome("result", "two issues");

        let text = read_back(dir.path(), "audit");
        assert!(text.starts_with("=== subagent audit ===\n"), "got: {text}");
        assert!(text.contains("check the config"));
        assert!(text.contains("looking now"));
        assert!(text.contains("two issues"));
    }

    /// The parent's directory sits beside the parent's own transcript, and the
    /// header spells out the whole path the directories encode.
    #[test]
    fn a_nested_transcript_lands_in_its_parents_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ancestry = ["parent-a".to_string(), "scan".to_string()];
        let mut log = Transcript::open(dir.path(), &ancestry, "audit", None);
        log.record_prompt("check the config");

        let nested = dir.path().join("subagent-parent-a").join("subagent-scan");
        let text = read_back(&nested, "audit");
        assert!(
            text.starts_with("=== subagent parent-a/scan/audit ===\n"),
            "got: {text}"
        );
        assert!(text.contains("check the config"));
    }

    /// The deepest tree the config allows, every name at the length limit,
    /// still gets its transcript: each ancestor adds a directory rather than
    /// lengthening one file name past the 255 bytes a path component may hold.
    #[test]
    fn the_deepest_tree_still_gets_a_transcript() {
        let dir = tempfile::tempdir().expect("tempdir");
        let name = "a".repeat(crate::subagent::MAX_NAME_LEN);
        // The primary is depth 1, so subagents fill one layer fewer than the
        // ceiling, and every layer but the last is an ancestor.
        let layers = outrig::config::SUBAGENT_DEPTH_MAX_CEILING as usize - 1;
        let ancestry = vec![name.clone(); layers - 1];
        let mut log = Transcript::open(dir.path(), &ancestry, &name, None);
        log.record_prompt("deep");

        let mut nested = dir.path().to_path_buf();
        for parent in &ancestry {
            nested.push(format!("subagent-{parent}"));
        }
        assert!(read_back(&nested, &name).contains("deep"));
    }

    #[test]
    fn rounds_append_rather_than_truncate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None);
        log.record_prompt("first");
        log.record_prompt("second");

        let text = read_back(dir.path(), "audit");
        assert!(text.contains("first") && text.contains("second"));
    }

    /// A name released and launched again appends to the same file, under a
    /// header of its own and below everything the first launch wrote (#307).
    #[test]
    fn a_relaunch_appends_below_everything_the_first_launch_wrote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut first = Transcript::open(dir.path(), &[], "audit", None);
        first.record_prompt("check the config");
        first.record_outcome("result", "two issues");
        // Release aborts the first subagent's task, which drops its transcript.
        drop(first);
        let mut second = Transcript::open(dir.path(), &[], "audit", None);
        second.record_prompt("check it again");

        assert_eq!(
            read_back(dir.path(), "audit"),
            "=== subagent audit ===\n\
             \n=== prompt ===\ncheck the config\n\
             \n--- result ---\ntwo issues\n\
             === subagent audit ===\n\
             \n=== prompt ===\ncheck it again\n"
        );
    }

    fn candidate(name: &str, provider: &str, identifier: &str) -> ModelLabel {
        ModelLabel {
            name: name.to_string(),
            provider: provider.to_string(),
            identifier: identifier.to_string(),
        }
    }

    fn chain() -> Vec<ModelLabel> {
        vec![
            candidate("head-model", "head", "claude-sonnet-4-6"),
            candidate("next-model", "next", "claude-opus-4-7"),
        ]
    }

    /// The block names every model that answered, in the shape the header uses
    /// for the one it started on.
    #[test]
    fn records_the_models_that_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None);
        log.record_answered(&chain(), &[0, 1]);

        assert_eq!(
            read_back(dir.path(), "audit"),
            "=== subagent audit ===\n\
             \n--- answered by ---\n\
             model: head-model / provider: head / claude-sonnet-4-6\n\
             model: next-model / provider: next / claude-opus-4-7\n"
        );
    }

    /// A subagent that never left its head -- or never had a chain -- has
    /// nothing to record, and its transcript stays byte for byte what it was.
    #[test]
    fn a_round_the_head_answered_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None);
        log.record_answered(&chain(), &[0]);
        log.record_answered(&chain(), &[]);

        assert_eq!(read_back(dir.path(), "audit"), "=== subagent audit ===\n");
    }

    /// The streaming path returns an empty reply for the primary agent, and a
    /// subagent whose outcome came only from `set_result` has nothing to add
    /// here -- an empty `--- reply ---` block would just be noise.
    #[test]
    fn an_empty_reply_writes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut log = Transcript::open(dir.path(), &[], "audit", None);
        log.record_reply("");

        assert!(
            !read_back(dir.path(), "audit").contains("reply"),
            "an empty reply should not open a block"
        );
    }

    /// Logging is best-effort: a subagent whose transcript cannot be opened
    /// still runs. Creating the dir under a *file* makes `create_dir_all` fail.
    #[test]
    fn an_unusable_log_dir_degrades_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("write file");

        let mut log = Transcript::open(&blocker, &[], "audit", None);
        log.record_prompt("check the config");
        log.record_outcome("result", "found things");
    }
}
