//! `outrig clean` -- bulk-remove old session records and stray containers.
//!
//! Two sweeps run together, both answered from a single unfiltered `podman ps
//! -a`. The session-store walk removes old metadata/log directories, refusing
//! to touch a session whose recorded container name is among the running
//! containers -- including an attach session's borrowed, outrig-unlabeled
//! container, which a label-filtered listing would miss. The label sweep
//! catches *stray* containers -- ones carrying `org.outrig.session` whose
//! session record is gone (lost record, SIGKILLed outrig, failed `--rm`) --
//! and `podman rm -f`s the stopped ones older than the cutoff in one
//! invocation. Running containers are never removed, labeled or not.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use clap::Parser;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::{OutrigError, Result};
use crate::session::{self, Session, SessionStore};
use outrig::container::{LABEL_SESSION, LABEL_SIDECAR};

const DAY: u64 = 24 * 60 * 60;
#[cfg_attr(not(feature = "internal-test-api"), allow(dead_code))]
pub const DEFAULT_OLDER_THAN: Duration = Duration::from_secs(30 * DAY);

#[derive(Debug, Parser)]
pub struct CleanArgs {
    /// Remove sessions older than this duration. Supports s, m, h, and d.
    #[arg(
        long = "older-than",
        value_name = "DURATION",
        default_value = "30d",
        value_parser = parse_duration
    )]
    pub older_than: Duration,
    /// Skip the interactive `[y/N]` confirmation.
    #[arg(short = 'y', long = "yes")]
    pub yes: bool,
}

/// One `org.outrig.session`-labeled podman container, as reported by
/// `podman ps -a`. Input to the stray sweep.
#[derive(Debug, Clone)]
pub struct LabeledContainer {
    pub name: String,
    pub session_label: String,
    pub sidecar_label: Option<String>,
    pub running: bool,
    pub created: SystemTime,
}

pub async fn execute(
    args: &CleanArgs,
    session_root_flag: Option<&Path>,
    repo_cfg_override: Option<&Path>,
    global_cfg_path: &Path,
    cwd: &Path,
) -> Result<i32> {
    let root = session::resolve_session_root_for_cli(
        session_root_flag,
        repo_cfg_override,
        global_cfg_path,
        cwd,
    )?;
    let store = SessionStore::new(root);
    let (labeled, running) = list_all_containers().await?;
    let stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stderr = tokio::io::stderr();
    execute_with(
        &mut stderr,
        stdin,
        &store,
        args,
        SystemTime::now(),
        running,
        labeled,
        podman_remove_force_batch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_with<E, R, D, DFut>(
    stderr: &mut E,
    stdin: R,
    store: &SessionStore,
    args: &CleanArgs,
    now: SystemTime,
    running: BTreeSet<String>,
    labeled: Vec<LabeledContainer>,
    mut remove_containers: D,
) -> Result<i32>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    D: FnMut(Vec<String>) -> DFut,
    DFut: Future<Output = Result<()>>,
{
    let sessions = store.list()?;
    let mut targets = Vec::new();
    let mut skipped_running = Vec::new();

    for session in &sessions {
        if !older_than(session, args.older_than, now) {
            continue;
        }
        if running.contains(&session.container_name) {
            skipped_running.push(session.clone());
            continue;
        }
        targets.push(CleanTarget {
            dir: session.session_dir.clone(),
            session: session.clone(),
        });
    }

    let target_ids: BTreeSet<&str> = targets.iter().map(|t| t.session.id.as_str()).collect();
    let (stray_targets, stray_running) =
        classify_strays(&sessions, &target_ids, labeled, args.older_than, now);

    write_skipped_running(stderr, &skipped_running).await?;
    write_stray_running(stderr, &stray_running).await?;

    let retention = format_retention(args.older_than);
    if targets.is_empty() && stray_targets.is_empty() {
        let msg =
            format!("[outrig] no stopped sessions or stray containers older than {retention}\n");
        stderr.write_all(msg.as_bytes()).await?;
        return Ok(0);
    }

    write_preview(stderr, &targets, &retention).await?;
    write_stray_preview(stderr, &stray_targets, &retention).await?;

    if !args.yes && !confirm(stderr, stdin, targets.len(), stray_targets.len()).await? {
        stderr.write_all(b"[outrig] aborted\n").await?;
        return Ok(0);
    }

    let mut removed = 0usize;
    for target in &targets {
        store.remove_by_id(&target.session.id)?;
        removed += 1;

        let dir_msg = format!("[outrig] removed {}\n", target.dir.display());
        stderr.write_all(dir_msg.as_bytes()).await?;
        if target.session.link_target.is_some() {
            let link_path = store.symlink_path(&target.session.id);
            let link_msg = format!("[outrig] removed {} (symlink)\n", link_path.display());
            stderr.write_all(link_msg.as_bytes()).await?;
        }
    }

    // One `podman rm -f` for every stray. Guard the empty case -- `podman rm
    // -f` with no names is an error -- and print the same per-container line
    // for each on success, so the happy path stays byte-identical.
    let strays_removed = stray_targets.len();
    if !stray_targets.is_empty() {
        remove_containers(stray_targets.iter().map(|s| s.name.clone()).collect()).await?;
        for stray in &stray_targets {
            let msg = format!("[outrig] removed container {}\n", stray.name);
            stderr.write_all(msg.as_bytes()).await?;
        }
    }

    let summary = format!(
        "[outrig] cleaned {}\n",
        clean_summary(removed, strays_removed)
    );
    stderr.write_all(summary.as_bytes()).await?;
    Ok(0)
}

pub fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let raw = value.trim();
    if raw.len() < 2 {
        return Err("expected a duration like 30d, 12h, 45m, or 10s".to_string());
    }

    let (amount, unit) = raw.split_at(raw.len() - 1);
    if amount.is_empty() || !amount.bytes().all(|b| b.is_ascii_digit()) {
        return Err("duration amount must be a positive integer".to_string());
    }

    let amount: u64 = amount
        .parse()
        .map_err(|_| "duration amount is too large".to_string())?;
    if amount == 0 {
        return Err("duration amount must be greater than zero".to_string());
    }

    let unit_secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => DAY,
        _ => return Err("duration unit must be one of s, m, h, or d".to_string()),
    };

    let secs = amount
        .checked_mul(unit_secs)
        .ok_or_else(|| "duration is too large".to_string())?;
    Ok(Duration::from_secs(secs))
}

struct CleanTarget {
    session: Session,
    dir: PathBuf,
}

/// Split labeled containers into `(removable strays, running strays)`.
/// A stray is a labeled container with no *surviving* session record -- a
/// record being removed this run counts as gone, so a failed `--rm` and its
/// record clean up together. Running containers are never removable, and
/// stopped strays must be older than the cutoff (podman `Created` time).
fn classify_strays(
    sessions: &[Session],
    removed_ids: &BTreeSet<&str>,
    labeled: Vec<LabeledContainer>,
    older_than: Duration,
    now: SystemTime,
) -> (Vec<LabeledContainer>, Vec<LabeledContainer>) {
    let surviving_ids: BTreeSet<&str> = sessions
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| !removed_ids.contains(id))
        .collect();
    let mut removable = Vec::new();
    let mut running = Vec::new();
    for container in labeled {
        if surviving_ids.contains(container.session_label.as_str()) {
            continue; // the record walk owns record-backed containers
        }
        if container.running {
            running.push(container);
        } else if now
            .duration_since(container.created)
            .map(|age| age >= older_than)
            .unwrap_or(false)
        {
            removable.push(container);
        }
    }
    (removable, running)
}

fn older_than(session: &Session, cutoff: Duration, now: SystemTime) -> bool {
    let age_basis = session.ended_at.unwrap_or(session.started_at);
    now.duration_since(age_basis)
        .map(|age| age >= cutoff)
        .unwrap_or(false)
}

async fn write_skipped_running<E>(stderr: &mut E, skipped: &[Session]) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if skipped.is_empty() {
        return Ok(());
    }

    stderr
        .write_all(b"[outrig] skipped running sessions:\n")
        .await?;
    for session in skipped {
        let line = format!("  {}  {}\n", session.id, session.container_name);
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn write_preview<E>(stderr: &mut E, targets: &[CleanTarget], retention: &str) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    let header = format!(
        "[outrig] will remove {} older than {retention}:\n",
        session_count(targets.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for target in targets {
        let (label, timestamp) = match target.session.ended_at {
            Some(t) => ("ended", t),
            None => ("started", target.session.started_at),
        };
        let line = format!(
            "  {}  {} {}  {}\n",
            target.session.id,
            label,
            session::format_started_at(timestamp),
            target.dir.display()
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn confirm<E, R>(stderr: &mut E, mut stdin: R, sessions: usize, strays: usize) -> Result<bool>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let prompt = format!("Clean {}? [y/N]: ", clean_summary(sessions, strays));
    stderr.write_all(prompt.as_bytes()).await?;
    stderr.flush().await?;
    let mut line = String::new();
    stdin.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// `"2 sessions"`, `"3 stray containers"`, or `"2 sessions and 3 stray
/// containers"`.
fn clean_summary(sessions: usize, strays: usize) -> String {
    let stray_part = |count: usize| {
        format!(
            "{count} stray {}",
            crate::cli::session_setup::plural(count, "container", "containers")
        )
    };
    match (sessions, strays) {
        (_, 0) => session_count(sessions),
        (0, s) => stray_part(s),
        (n, s) => format!("{} and {}", session_count(n), stray_part(s)),
    }
}

async fn write_stray_running<E>(stderr: &mut E, strays: &[LabeledContainer]) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if strays.is_empty() {
        return Ok(());
    }
    stderr
        .write_all(b"[outrig] skipped running labeled containers (no session record):\n")
        .await?;
    for stray in strays {
        let line = format!("  {}  session {}\n", stray.name, stray.session_label);
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn write_stray_preview<E>(
    stderr: &mut E,
    strays: &[LabeledContainer],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if strays.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] will remove {} older than {retention} (no session record):\n",
        clean_summary(0, strays.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for stray in strays {
        let role = match &stray.sidecar_label {
            Some(sc) => format!("sidecar {sc}"),
            None => "primary".to_string(),
        };
        let line = format!(
            "  {}  {role}, session {}\n",
            stray.name, stray.session_label
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn session_count(count: usize) -> String {
    if count == 1 {
        "1 session".to_string()
    } else {
        format!("{count} sessions")
    }
}

fn format_retention(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs.is_multiple_of(DAY) {
        format!("{}d", secs / DAY)
    } else if secs.is_multiple_of(60 * 60) {
        format!("{}h", secs / (60 * 60))
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// `podman rm -f <name>...` for every stray in one invocation, propagating
/// failure -- clean should report a container it could not remove rather than
/// claiming success. Callers guard the empty case (`podman rm -f` with no
/// names is an error).
async fn podman_remove_force_batch(names: Vec<String>) -> Result<()> {
    let output = tokio::process::Command::new("podman")
        .args(["rm", "-f"])
        .args(&names)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(OutrigError::Configuration(format!(
            "podman rm -f {} failed: {}",
            names.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(())
}

/// `podman ps -a --format json`, decoded into the labeled-container rows (the
/// stray-sweep input) plus the set of *all* running container names (the
/// record-walk running check). Unfiltered so an attach session's borrowed,
/// outrig-unlabeled primary still shows up in the running set. Rows missing the
/// expected fields are skipped rather than failing the sweep.
async fn list_all_containers() -> Result<(Vec<LabeledContainer>, BTreeSet<String>)> {
    let output = tokio::process::Command::new("podman")
        .args(["ps", "-a", "--format", "json"])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        return Err(OutrigError::Configuration(format!(
            "podman ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    parse_all_containers(&output.stdout)
}

fn parse_all_containers(stdout: &[u8]) -> Result<(Vec<LabeledContainer>, BTreeSet<String>)> {
    let rows: Vec<serde_json::Value> = serde_json::from_slice(stdout).map_err(|source| {
        OutrigError::Configuration(format!("podman ps --format json: invalid JSON: {source}"))
    })?;
    let mut labeled = Vec::new();
    let mut running = BTreeSet::new();
    for row in rows {
        let Some(name) = row
            .get("Names")
            .and_then(|names| names.as_array())
            .and_then(|names| names.first())
            .and_then(|name| name.as_str())
        else {
            continue;
        };
        let is_running = row
            .get("State")
            .and_then(|state| state.as_str())
            .is_some_and(|state| state.eq_ignore_ascii_case("running"));
        if is_running {
            running.insert(name.to_string());
        }
        let labels = row.get("Labels").and_then(|labels| labels.as_object());
        let Some(session_label) = labels
            .and_then(|labels| labels.get(LABEL_SESSION))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        let sidecar_label = labels
            .and_then(|labels| labels.get(LABEL_SIDECAR))
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let created = row
            .get("Created")
            .and_then(|created| created.as_i64())
            .map(|secs| SystemTime::UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        labeled.push(LabeledContainer {
            name: name.to_string(),
            session_label: session_label.to_string(),
            sidecar_label,
            running: is_running,
            created,
        });
    }
    Ok((labeled, running))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_all_containers_splits_labeled_rows_and_running_names() {
        let stdout = br#"[
          {
            "Names": ["outrig-abc"],
            "Labels": {"org.outrig.session": "abc"},
            "State": "running",
            "Created": 1700000000
          },
          {
            "Names": ["outrig-abc-tools"],
            "Labels": {"org.outrig.session": "abc", "org.outrig.sidecar": "tools"},
            "State": "exited",
            "Created": 1700000100
          },
          {
            "Names": ["unlabeled-stopped"],
            "Labels": {},
            "State": "exited",
            "Created": 1700000200
          },
          {
            "Names": ["borrowed-attach-container"],
            "Labels": {},
            "State": "running",
            "Created": 1700000300
          }
        ]"#;

        let (labeled, running) = parse_all_containers(stdout).expect("parse");

        // Only the two `org.outrig.session` rows are stray-sweep input.
        assert_eq!(labeled.len(), 2, "unlabeled containers are not stray input");
        assert_eq!(labeled[0].name, "outrig-abc");
        assert_eq!(labeled[0].session_label, "abc");
        assert_eq!(labeled[0].sidecar_label, None);
        assert!(labeled[0].running);
        assert_eq!(
            labeled[0].created,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
        );
        assert_eq!(labeled[1].name, "outrig-abc-tools");
        assert_eq!(labeled[1].sidecar_label.as_deref(), Some("tools"));
        assert!(!labeled[1].running);

        // The running set spans every running container, labeled or not, so an
        // attach session's unlabeled primary is still seen as alive.
        assert_eq!(running.len(), 2);
        assert!(running.contains("outrig-abc"));
        assert!(running.contains("borrowed-attach-container"));
        assert!(!running.contains("outrig-abc-tools"), "exited sidecar");
        assert!(!running.contains("unlabeled-stopped"));
    }

    #[test]
    fn parse_all_containers_rejects_bad_json() {
        assert!(parse_all_containers(b"not json").is_err());
    }
}
