//! The log conmon keeps for an entrypoint-stdio container, which holds the stderr that
//! `podman start --attach` can fail to relay.
//!
//! For a container that exits within milliseconds, the attach client can return the exit code and
//! none of the output (#422). outrig starts the MCP handshake when it spawns the client, so the
//! client always has a stdin message to forward. If conmon closes the attach socket with that
//! message unread, the kernel reports `ECONNRESET` on podman's end ahead of the stderr already
//! queued there, and podman reads the reset as the end of the stream. (That chain is read from the
//! sources of podman 4.9 and 5.8, conmon 2.1 and linux 6.17; the race has not been reproduced.)
//! conmon writes a container's output to its log before it writes any attach socket, and drains it
//! before it closes one, so the log has the line.
//!
//! podman's own log is out of reach. It lives in the container's `userdata`, which podman gives to
//! the container's root -- a subordinate uid, mode 0700 -- for every `--userns=keep-id` and
//! `--userns=container:` container, and `--rm` removes it with the container. So the log is
//! directed to a file in a directory of outrig's own.
//!
//! This is a diagnostic. [`CtrLog::create`] declines rather than fails, and a container without
//! one reports exactly what it did before.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};

/// How much of the end of a log [`stderr_text`] reads. A startup failure writes a few lines.
const TAIL: u64 = 64 * 1024;

/// The file an entrypoint-stdio container's log is written to, in a directory only this user can
/// enter. conmon creates the file when the container is initialized, and dropping this removes it
/// and the directory.
#[derive(Debug)]
pub(crate) struct CtrLog {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl CtrLog {
    /// A log for one container, or `None` if there is no place to put one.
    pub(crate) fn create() -> Option<Self> {
        // The runtime directory where there is one: a tmpfs, so the protocol stream the log also
        // holds never reaches a disk, and conmon's `fsync` of it at exit is free.
        let parent = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let dir = tempfile::Builder::new()
            .prefix("outrig-ctr-log-")
            .permissions(Permissions::from_mode(0o700))
            .tempdir_in(parent)
            .inspect_err(|e| {
                tracing::debug!(target: "outrig::container", "no container log: {e}");
            })
            .ok()?;
        let path = dir.path().join("ctr.log");
        if !podman_takes_verbatim(&path) {
            tracing::debug!(
                target: "outrig::container",
                "no container log: podman would not take {} as one value",
                path.display()
            );
            return None;
        }
        Some(Self { _dir: dir, path })
    }

    /// Where conmon is told to write: the value of `--log-opt=path=`.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// Whether podman and conmon take `<path>` whole. podman 4.9 reads `--log-opt=path=<path>` as a
/// comma-separated list (5.x does not), so a comma, or a quote that would change how the list is
/// read, would split it. conmon is handed `k8s-file:<path>` and splits that at colons, so a colon
/// would cut the path short: it would log to the part before the colon, which is not the private
/// directory and is not removed with it. Any of them can be in `$XDG_RUNTIME_DIR` or `$TMPDIR`. A
/// path that is not UTF-8 is not offered either.
fn podman_takes_verbatim(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| !path.contains([',', '"', ':']))
}

/// What the container wrote to its stderr, from the last [`TAIL`] bytes of the log at `path`.
pub(crate) async fn stderr_text(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let start = file.metadata().await?.len().saturating_sub(TAIL);
    // From one byte early when that is not the start of the file: it is a newline exactly when
    // the window opens on an entry, and not partway through one.
    file.seek(SeekFrom::Start(start.saturating_sub(1))).await?;
    let mut tail = Vec::new();
    file.take(TAIL + 1).read_to_end(&mut tail).await?;
    // Through the first newline is the cut-off end of an entry, or nothing.
    let entries = match tail.iter().position(|&byte| byte == b'\n') {
        Some(newline) if start > 0 => &tail[newline + 1..],
        None if start > 0 => &[],
        _ => &tail[..],
    };
    Ok(stderr_text_of(entries))
}

/// The stderr text in `log`, which holds podman's `k8s-file` entries:
/// `<RFC 3339 time> <stdout|stderr> <P|F> <text>`, one per line.
///
/// conmon writes one entry for each read of the container's pipe, and a read that ends before a
/// newline is a `P` entry, so a line written in several pieces is several entries: `P` for each
/// piece, `F` for the one that ends it. The text of a `P` entry runs on into the next, and an `F`
/// entry's text ends the line. The bytes are joined before they are decoded, so a character
/// split across two reads survives.
fn stderr_text_of(log: &[u8]) -> String {
    let mut stderr = Vec::new();
    for entry in log.split(|&byte| byte == b'\n') {
        let mut fields = entry.splitn(4, |&byte| byte == b' ');
        let (Some(_time), Some(b"stderr"), Some(tag), Some(text)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        stderr.extend_from_slice(text);
        if tag == b"F" {
            stderr.push(b'\n');
        }
    }
    String::from_utf8_lossy(&stderr).into_owned()
}

/// What a test needs to hold a log without a container: shared with `mcp`'s tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::CtrLog;

    /// One `k8s-file` entry, as conmon writes it.
    pub(crate) fn entry(stream: &str, tag: &str, text: &str) -> String {
        format!("2026-10-13T14:30:00.123456789-06:00 {stream} {tag} {text}\n")
    }

    /// A log holding `contents`, in the file conmon would have made.
    pub(crate) fn log_holding(contents: &str) -> CtrLog {
        let log = CtrLog::create().expect("a directory to put the log in");
        std::fs::write(log.path(), contents).expect("write the log");
        log
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{entry, log_holding};
    use super::*;

    /// The launcher's `eprintln!` is unbuffered, so one diagnostic is several writes, and
    /// conmon logs each read it makes of them.
    #[test]
    fn partial_entries_join_into_the_line_the_full_entry_ends() {
        let log = [
            entry("stderr", "P", "outrig-enter: "),
            entry("stderr", "P", "open outrig-no-such-program"),
            entry(
                "stderr",
                "F",
                ": No such file or directory (not found in PATH=/usr/bin)",
            ),
        ]
        .concat();

        assert_eq!(
            stderr_text_of(log.as_bytes()),
            "outrig-enter: open outrig-no-such-program: No such file or directory \
             (not found in PATH=/usr/bin)\n"
        );
    }

    #[test]
    fn only_stderr_entries_are_stderr() {
        let log = [
            entry("stdout", "F", r#"{"jsonrpc":"2.0","id":0}"#),
            entry("stderr", "F", "first"),
            entry("stdout", "P", "protocol"),
            entry("stderr", "F", "second"),
        ]
        .concat();

        assert_eq!(stderr_text_of(log.as_bytes()), "first\nsecond\n");
    }

    #[test]
    fn a_trailing_partial_entry_is_still_text() {
        let log = [
            entry("stderr", "F", "done"),
            entry("stderr", "P", "cut off mid-wri"),
        ]
        .concat();

        assert_eq!(stderr_text_of(log.as_bytes()), "done\ncut off mid-wri");
    }

    #[test]
    fn a_blank_stderr_line_is_kept() {
        let log = [
            entry("stderr", "F", "before"),
            entry("stderr", "F", ""),
            entry("stderr", "F", "after"),
        ]
        .concat();

        assert_eq!(stderr_text_of(log.as_bytes()), "before\n\nafter\n");
    }

    /// `é` is two bytes. Decoding each entry on its own would turn both halves into U+FFFD.
    #[test]
    fn a_character_split_across_partial_entries_survives() {
        let mut log = Vec::new();
        log.extend_from_slice(b"2026-10-13T14:30:00.1-06:00 stderr P caf\xc3\n");
        log.extend_from_slice(b"2026-10-13T14:30:00.2-06:00 stderr F \xa9 closed\n");

        assert_eq!(stderr_text_of(&log), "café closed\n");
    }

    #[test]
    fn lines_that_are_not_log_entries_are_skipped() {
        let log = format!(
            "not an entry\n\n2026-10-13T14:30:00-06:00 stderr\n{}one-token\n",
            entry("stderr", "F", "kept"),
        );

        assert_eq!(stderr_text_of(log.as_bytes()), "kept\n");
    }

    #[tokio::test]
    async fn what_the_container_wrote_is_read_from_the_file() {
        let log = log_holding(
            &[
                entry("stdout", "F", "protocol"),
                entry("stderr", "F", "boom"),
            ]
            .concat(),
        );

        assert_eq!(stderr_text(log.path()).await.unwrap(), "boom\n");
    }

    #[tokio::test]
    async fn a_log_with_nothing_in_it_has_no_stderr() {
        let log = log_holding("");

        assert_eq!(stderr_text(log.path()).await.unwrap(), "");
    }

    /// conmon makes the file when the container is initialized; until then there is nothing to read.
    #[tokio::test]
    async fn a_log_conmon_has_not_made_is_an_error() {
        let log = CtrLog::create().expect("a directory to put the log in");

        assert!(stderr_text(log.path()).await.is_err());
    }

    /// An entry exactly 64 bytes long, so that [`TAIL`] is a whole number of them.
    fn whole_entry(number: usize) -> String {
        let entry = entry("stderr", "F", &format!("line {number:013}"));
        assert_eq!(entry.len(), 64);
        entry
    }

    /// Only the end of the log is read. Where that begins is usually inside an entry, and a
    /// fragment is dropped rather than parsed as if it were whole.
    #[tokio::test]
    async fn a_log_longer_than_the_tail_starts_at_an_entry() {
        let entries = (TAIL / 64) as usize + 7;
        // The last entry is ten bytes longer, so the window opens ten bytes into an entry.
        let log = log_holding(&format!(
            "{}{}",
            (0..entries - 1).map(whole_entry).collect::<String>(),
            entry(
                "stderr",
                "F",
                &format!("line {:013} and ten more", entries - 1)
            ),
        ));

        let text = stderr_text(log.path()).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();

        let kept = (TAIL / 64) as usize - 1;
        assert_eq!(lines.len(), kept, "the entry the window cut is not here");
        assert_eq!(lines[0], format!("line {:013}", entries - kept));
        assert_eq!(
            lines[kept - 1],
            format!("line {:013} and ten more", entries - 1)
        );
    }

    /// The window opens on an entry's first byte exactly: nothing is cut, so nothing is dropped.
    #[tokio::test]
    async fn a_window_that_opens_on_an_entry_keeps_it() {
        let entries = (TAIL / 64) as usize + 7;
        let log = log_holding(&(0..entries).map(whole_entry).collect::<String>());

        let text = stderr_text(log.path()).await.unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert_eq!(lines.len(), (TAIL / 64) as usize);
        assert_eq!(lines[0], format!("line {:013}", 7));
        assert_eq!(
            lines.last().copied(),
            Some(format!("line {:013}", entries - 1).as_str())
        );
    }

    #[test]
    fn the_log_is_private_to_its_owner_and_goes_with_its_handle() {
        let log = CtrLog::create().expect("a directory to put the log in");
        let dir = log
            .path()
            .parent()
            .expect("a directory of its own")
            .to_path_buf();

        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "the protocol stream is in the log");

        drop(log);
        assert!(!dir.exists(), "nothing is left behind");
    }

    /// podman 4.9 reads `--log-opt` as a comma-separated list, so a path with a comma in it
    /// would reach it as two options and fail the create.
    #[test]
    fn a_path_podman_would_split_is_not_used() {
        assert!(podman_takes_verbatim(Path::new(
            "/run/user/1000/outrig-ctr-log-a1b2/ctr.log"
        )));
        assert!(!podman_takes_verbatim(Path::new(
            "/run/user/1000/a,b/ctr.log"
        )));
        assert!(!podman_takes_verbatim(Path::new("/tmp/\"quoted\"/ctr.log")));
    }

    /// conmon splits `k8s-file:<path>` at colons, so `/tmp/a:b/...` would be logged to `/tmp/a`.
    #[test]
    fn a_path_conmon_would_cut_short_is_not_used() {
        assert!(!podman_takes_verbatim(Path::new("/tmp/a:b/ctr.log")));
        assert!(!podman_takes_verbatim(Path::new("/run/user/1000/ctr.log:")));
    }
}
