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
//! invocation. Running containers are never removed, labeled or not, and
//! neither is one whose creation time cannot be read: with no age to measure
//! against the cutoff, it is reported instead.
//!
//! `--build-containers` adds a third sweep, off by default and answered from
//! its own listings. Separate for a reason beyond tidiness: buildah working
//! containers are not podman containers and never appear in `podman ps -a`,
//! so folding them into that listing would feed the other two sweeps rows
//! they have no way to reason about. See [`crate::cli::build_containers`]
//! for why it is opt-in.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use clap::Parser;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::build_containers::{
    self, BuildContainer, buildah_remove_batch, classify_build_containers,
};
use crate::cli::engine;
use crate::error::{OutrigError, Result};
use crate::session::{self, DanglingLink, Session, SessionId, SessionStore, SkippedSession};
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
    /// Restrict both sweeps to one session id.
    ///
    /// Only this session's record is considered for removal, and only
    /// containers labeled `org.outrig.session=<ID>` are stray candidates.
    /// Without it `clean` sweeps every session in the store and every
    /// labeled container on the machine -- including containers a
    /// *different* session root owns, whose records this invocation cannot
    /// see and therefore reads as strays.
    #[arg(long = "session", value_name = "ID")]
    pub session: Option<String>,
    /// Also remove buildah working containers left by interrupted builds.
    ///
    /// Unlike the other sweeps this one is not label-scoped -- buildah offers
    /// no way to mark these -- so it removes *every* buildah working
    /// container older than the cutoff, including any you made yourself.
    #[arg(long = "build-containers")]
    pub build_containers: bool,
    /// Also remove session records outrig cannot read.
    ///
    /// A record that does not parse -- hand-edited, truncated, or written by
    /// an outrig whose schema differs -- has no end time to age, so it is
    /// aged by the start time in its directory name; a directory whose name is
    /// not a session id is never removed. `--yes` alone leaves these in place
    /// and says so, since what is in them cannot be shown.
    #[arg(long = "unreadable")]
    pub unreadable: bool,
}

/// One `org.outrig.session`-labeled podman container, as reported by
/// `podman ps -a`. Input to the stray sweep.
#[derive(Debug, Clone)]
pub struct LabeledContainer {
    pub name: String,
    pub session_label: String,
    pub sidecar_label: Option<String>,
    pub running: bool,
    /// `None` when the row's `Created` is not a readable time: absent, neither
    /// an integer nor an RFC 3339 string, or at or before the epoch. Such a
    /// container is reported and left alone: an unknown age cannot clear a
    /// cutoff, and the container may be seconds old.
    pub created: Option<SystemTime>,
}

pub async fn execute(
    args: &CleanArgs,
    session_root_flag: Option<&Path>,
    global_override: Option<&Path>,
) -> Result<i32> {
    let root = session::resolve_session_root_for_cli(session_root_flag, global_override)?;
    let store = SessionStore::new(root);
    // Only when asked: with the flag off, `clean` still needs nothing but
    // podman, and buildah is never invoked. When it is asked, the listings
    // are independent read-only queries and run together rather than end to
    // end.
    let ((labeled, running), build) = if args.build_containers {
        tokio::try_join!(
            list_all_containers(),
            build_containers::list_build_containers()
        )?
    } else {
        (list_all_containers().await?, Vec::new())
    };
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
        build,
        podman_remove_force_batch,
        buildah_remove_batch,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_with<E, R, D, DFut, B, BFut>(
    stderr: &mut E,
    stdin: R,
    store: &SessionStore,
    args: &CleanArgs,
    now: SystemTime,
    running: BTreeSet<String>,
    labeled: Vec<LabeledContainer>,
    build: Vec<BuildContainer>,
    mut remove_containers: D,
    mut remove_build_containers: B,
) -> Result<i32>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    // Returns the subset of `names` still in the store afterwards -- see the
    // call site for why the batch's exit status cannot answer that.
    D: FnMut(Vec<String>) -> DFut,
    DFut: Future<Output = Result<Vec<String>>>,
    B: FnMut(Vec<String>) -> BFut,
    BFut: Future<Output = Result<()>>,
{
    let listing = store.list()?;
    // `--session` narrows both sweeps together, which is the only way the
    // flag can mean one thing: the record walk decides what counts as a
    // surviving session, and the label sweep removes containers whose session
    // did not survive. Narrowing one without the other would make a scoped
    // run delete containers it did not preview.
    //
    // `listing.skipped` is deliberately not narrowed. Those are directories
    // that would not parse, so their session id is exactly what is unknown;
    // they keep protecting a container that matches by name, which is the
    // conservative direction.
    let sessions: Vec<Session> = match args.session.as_deref() {
        Some(id) => listing
            .sessions
            .into_iter()
            .filter(|s| s.id.as_str() == id)
            .collect(),
        None => listing.sessions,
    };
    let labeled: Vec<LabeledContainer> = match args.session.as_deref() {
        Some(id) => labeled
            .into_iter()
            .filter(|c| c.session_label == id)
            .collect(),
        None => labeled,
    };
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

    // A link whose directory is gone has no record to age, so its own mtime --
    // set when the session started -- stands in for the start time.
    let mut dangling_targets = Vec::new();
    let mut dangling_running = Vec::new();
    for link in &listing.dangling {
        if args.session.as_deref().is_some_and(|id| id != link.entry) {
            continue;
        }
        let aged = now
            .duration_since(link.modified)
            .map(|age| age >= args.older_than)
            .unwrap_or(false);
        if !aged {
            continue;
        }
        if running.contains(&link.container_name()) {
            dangling_running.push(link);
        } else {
            dangling_targets.push(link);
        }
    }

    // A record we couldn't parse is aged by the start time in its name. Removal
    // is the `--unreadable` opt-in, so without it these are only reported; and
    // `--session`, which does not narrow `listing.skipped` above, narrows what
    // may be removed, so a scoped run removes nothing it was not pointed at.
    let retention = format_retention(args.older_than);
    let mut unreadable_targets = Vec::new();
    let mut unreadable_left = Vec::new();
    let mut unreadable_unsure = Vec::new();
    let mut unreadable_running = Vec::new();
    for skipped in &listing.skipped {
        let aged = SessionId::started_at(&skipped.entry).is_some_and(|started| {
            now.duration_since(started)
                .is_ok_and(|age| age >= args.older_than)
        });
        if !aged || args.session.as_deref().is_some_and(|id| id != skipped.entry) {
            continue;
        }
        if running.contains(&skipped.container_name()) {
            unreadable_running.push(skipped);
        } else if args.unreadable && !skipped.container_known() {
            // The record never got as far as naming its container, and an
            // attach session's is not `outrig-<sid>`: nothing here shows it
            // is not running, so it stays for `outrig discard` to ask about.
            unreadable_unsure.push(skipped);
        } else if args.unreadable {
            unreadable_targets.push(skipped);
        } else {
            unreadable_left.push(skipped);
        }
    }

    let mut target_ids: BTreeSet<&str> = targets.iter().map(|t| t.session.id.as_str()).collect();
    target_ids.extend(unreadable_targets.iter().map(|s| s.entry.as_str()));
    // A record we couldn't parse is still a record: its directory survives this
    // run unless removed above, so its container is not a stray and must not be
    // force-removed out from under it.
    let unreadable: Vec<&str> = listing
        .skipped
        .iter()
        .map(|s| s.entry.as_str())
        .filter(|entry| !target_ids.contains(entry))
        .collect();
    let (stray_targets, stray_running, stray_undatable) = classify_strays(
        &sessions,
        &target_ids,
        &unreadable,
        labeled,
        args.older_than,
        now,
    );

    let (build_targets, build_undatable) =
        classify_build_containers(build, args.older_than, now);

    write_skipped_running(
        stderr,
        &skipped_running,
        &dangling_running,
        &unreadable_running,
    )
    .await?;
    write_unreadable_left(stderr, &unreadable_left, &retention).await?;
    write_unreadable_unsure(stderr, &unreadable_unsure, &retention).await?;
    write_stray_skipped(
        stderr,
        "[outrig] skipped running labeled containers (no session record):\n",
        &stray_running,
    )
    .await?;
    write_stray_skipped(
        stderr,
        "[outrig] skipped labeled containers with no creation time (no session record):\n",
        &stray_undatable,
    )
    .await?;
    write_build_undatable(stderr, &build_undatable).await?;

    if targets.is_empty()
        && dangling_targets.is_empty()
        && unreadable_targets.is_empty()
        && stray_targets.is_empty()
        && build_targets.is_empty()
    {
        // The wording only grows when the sweep was asked for, so a run
        // without the flag reads exactly as it always has.
        let msg = if args.build_containers {
            format!(
                "[outrig] no stopped sessions, stray containers, or build containers \
                 older than {retention}\n"
            )
        } else {
            format!("[outrig] no stopped sessions or stray containers older than {retention}\n")
        };
        stderr.write_all(msg.as_bytes()).await?;
        return Ok(0);
    }

    write_preview(stderr, &targets, &retention).await?;
    write_dangling_preview(stderr, &dangling_targets, &retention).await?;
    write_unreadable_preview(stderr, &unreadable_targets, &retention).await?;
    write_stray_preview(stderr, &stray_targets, &retention).await?;
    write_build_preview(stderr, &build_targets, &retention).await?;

    if !args.yes
        && !confirm(
            stderr,
            stdin,
            targets.len() + dangling_targets.len() + unreadable_targets.len(),
            stray_targets.len(),
            build_targets.len(),
        )
        .await?
    {
        stderr.write_all(b"[outrig] aborted\n").await?;
        return Ok(0);
    }

    let mut removed = 0usize;
    for target in &targets {
        // What the session's directory still holds -- files outrig did not
        // write, which stay. Not a failure, unlike `kept` below.
        let left = store.remove_by_id(&target.session.id)?;
        removed += 1;

        let dir_msg = super::removal_report(&target.dir, &left);
        stderr.write_all(dir_msg.as_bytes()).await?;
        if target.session.link_target.is_some() {
            let link_path = store.symlink_path(&target.session.id);
            let link_msg = format!("[outrig] removed {} (symlink)\n", link_path.display());
            stderr.write_all(link_msg.as_bytes()).await?;
        }
    }

    for link in &dangling_targets {
        let (gone, msg) = super::unlink_dangling_report(store, link)?;
        removed += usize::from(gone);
        stderr.write_all(msg.as_bytes()).await?;
    }

    for skipped in &unreadable_targets {
        let left = store.remove_by_id(&skipped.id())?;
        removed += 1;
        let dir_msg = super::removal_report(&skipped.dir, &left);
        stderr.write_all(dir_msg.as_bytes()).await?;
        if skipped.linked {
            let link_path = store.symlink_path(&skipped.id());
            let link_msg = format!("[outrig] removed {} (symlink)\n", link_path.display());
            stderr.write_all(link_msg.as_bytes()).await?;
        }
    }

    // One `podman rm -f` for every stray. Guard the empty case -- `podman rm
    // -f` with no names is an error.
    //
    // The per-container outcome comes from a re-read, not from the batch's
    // exit status. That status is one number for every name in the batch, and
    // podman returns zero having skipped a container that another process was
    // tearing down at the same moment -- measured, repeatedly, and it is
    // self-perpetuating: the container this claimed to remove joins the next
    // sweep's batch and is skipped again. A line saying a container was
    // removed is now a claim about that container.
    let kept: BTreeSet<String> = if stray_targets.is_empty() {
        BTreeSet::new()
    } else {
        remove_containers(stray_targets.iter().map(|s| s.name.clone()).collect())
            .await?
            .into_iter()
            .collect()
    };
    for stray in &stray_targets {
        let verb = if kept.contains(&stray.name) {
            "could not remove"
        } else {
            "removed"
        };
        let msg = format!("[outrig] {verb} container {}\n", stray.name);
        stderr.write_all(msg.as_bytes()).await?;
    }
    // `kept` is what the removal hook reports still present, so it is a subset
    // of the names it was handed and the subtraction cannot underflow.
    let strays_removed = stray_targets.len() - kept.len();

    // After the strays, so a `podman rm` that failed reports rather than
    // letting this claim a clean sweep on top of it.
    let builds_removed = build_targets.len();
    if !build_targets.is_empty() {
        remove_build_containers(build_targets.iter().map(|c| c.id.clone()).collect()).await?;
        for container in &build_targets {
            let msg = format!(
                "[outrig] removed build container {}\n",
                container.short_id()
            );
            stderr.write_all(msg.as_bytes()).await?;
        }
    }

    let summary = format!(
        "[outrig] cleaned {}\n",
        clean_summary(removed, strays_removed, builds_removed)
    );
    stderr.write_all(summary.as_bytes()).await?;
    // Reporting the failure is not enough on its own: a script that checks the
    // exit code would otherwise read a partial sweep as a complete one.
    Ok(if kept.is_empty() { 0 } else { 1 })
}

pub fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let mut chars = value.trim().chars();
    let unit = chars.next_back();
    let amount = chars.as_str();
    if amount.is_empty() {
        return Err("expected a duration like 30d, 12h, 45m, or 10s".to_string());
    }

    if !amount.bytes().all(|b| b.is_ascii_digit()) {
        return Err("duration amount must be a positive integer".to_string());
    }

    let amount: u64 = amount
        .parse()
        .map_err(|_| "duration amount is too large".to_string())?;
    if amount == 0 {
        return Err("duration amount must be greater than zero".to_string());
    }

    let unit_secs = match unit {
        Some('s') => 1,
        Some('m') => 60,
        Some('h') => 60 * 60,
        Some('d') => DAY,
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

/// Split labeled containers into `(removable, running, undatable)` strays.
/// A stray is a labeled container with no *surviving* session record -- a
/// record being removed this run counts as gone, so a failed `--rm` and its
/// record clean up together. `unreadable` names entries that exist on disk but
/// couldn't be parsed; they survive too, so their containers are record-backed
/// rather than stray. Running containers are never removable, and stopped
/// strays must be older than the cutoff (podman `Created` time). A stopped
/// stray with no readable `Created` is `undatable`, never removable: the
/// cutoff is what keeps a sweep off a container made a moment ago, and a
/// container that cannot be measured against it is reported instead.
fn classify_strays(
    sessions: &[Session],
    removed_ids: &BTreeSet<&str>,
    unreadable: &[&str],
    labeled: Vec<LabeledContainer>,
    older_than: Duration,
    now: SystemTime,
) -> (
    Vec<LabeledContainer>,
    Vec<LabeledContainer>,
    Vec<LabeledContainer>,
) {
    let surviving_ids: BTreeSet<&str> = sessions
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| !removed_ids.contains(id))
        .chain(unreadable.iter().copied())
        .collect();
    let mut removable = Vec::new();
    let mut running = Vec::new();
    let mut undatable = Vec::new();
    for container in labeled {
        if surviving_ids.contains(container.session_label.as_str()) {
            continue; // the record walk owns record-backed containers
        }
        if container.running {
            running.push(container);
        } else if let Some(created) = container.created {
            if now
                .duration_since(created)
                .is_ok_and(|age| age >= older_than)
            {
                removable.push(container);
            }
        } else {
            undatable.push(container);
        }
    }
    (removable, running, undatable)
}

fn older_than(session: &Session, cutoff: Duration, now: SystemTime) -> bool {
    let age_basis = session.ended_at.unwrap_or(session.started_at);
    now.duration_since(age_basis)
        .map(|age| age >= cutoff)
        .unwrap_or(false)
}

async fn write_skipped_running<E>(
    stderr: &mut E,
    skipped: &[Session],
    dangling: &[&DanglingLink],
    unreadable: &[&SkippedSession],
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if skipped.is_empty() && dangling.is_empty() && unreadable.is_empty() {
        return Ok(());
    }

    stderr
        .write_all(b"[outrig] skipped running sessions:\n")
        .await?;
    for session in skipped {
        let line = format!("  {}  {}\n", session.id, session.container_name);
        stderr.write_all(line.as_bytes()).await?;
    }
    for link in dangling {
        let line = format!("  {}  {}\n", link.entry, link.container_name());
        stderr.write_all(line.as_bytes()).await?;
    }
    for record in unreadable {
        let line = format!("  {}  {}\n", record.entry, record.container_name());
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

async fn write_unreadable_preview<E>(
    stderr: &mut E,
    records: &[&SkippedSession],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if records.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] will remove {} older than {retention} (record unreadable):\n",
        unreadable_count(records.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for record in records {
        let line = format!(
            "  {}  {}  {}\n",
            record.entry,
            record.dir.display(),
            record.reason()
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

/// The records `--unreadable` would remove, named rather than silently kept:
/// `--yes` does not answer for what cannot be shown.
async fn write_unreadable_left<E>(
    stderr: &mut E,
    records: &[&SkippedSession],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if records.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] kept {} older than {retention}; --unreadable removes them:\n",
        unreadable_count(records.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for record in records {
        let line = format!("  {}  {}\n", record.entry, record.reason());
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

/// Records `--unreadable` would remove but for not knowing their container.
async fn write_unreadable_unsure<E>(
    stderr: &mut E,
    records: &[&SkippedSession],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if records.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] kept {} older than {retention}: the damaged record does not say which \
         container it owns, so a running attach session cannot be ruled out; \
         `outrig discard <sid>` removes one:\n",
        unreadable_count(records.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for record in records {
        let line = format!("  {}  {}\n", record.entry, record.reason());
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn unreadable_count(count: usize) -> String {
    format!(
        "{count} unreadable session {}",
        crate::cli::session_setup::plural(count, "record", "records")
    )
}

async fn write_dangling_preview<E>(
    stderr: &mut E,
    links: &[&DanglingLink],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if links.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] will remove {} older than {retention} (directory gone):\n",
        session_link_count(links.len())
    );
    stderr.write_all(header.as_bytes()).await?;
    for link in links {
        let line = format!("  {}  -> {}\n", link.entry, link.target.display());
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

fn session_link_count(count: usize) -> String {
    format!(
        "{count} session {}",
        crate::cli::session_setup::plural(count, "link", "links")
    )
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

async fn confirm<E, R>(
    stderr: &mut E,
    mut stdin: R,
    sessions: usize,
    strays: usize,
    builds: usize,
) -> Result<bool>
where
    E: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let prompt = format!("Clean {}? [y/N]: ", clean_summary(sessions, strays, builds));
    stderr.write_all(prompt.as_bytes()).await?;
    stderr.flush().await?;
    let mut line = String::new();
    stdin.read_line(&mut line).await?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// `"2 sessions"`, `"3 stray containers"`, `"2 sessions and 3 stray
/// containers"`, or all three joined `"a, b and c"`.
///
/// Zero-count parts drop out, so a run without `--build-containers` reads
/// exactly as it did before there was a third one.
fn clean_summary(sessions: usize, strays: usize, builds: usize) -> String {
    let mut parts = Vec::new();
    if sessions > 0 {
        parts.push(session_count(sessions));
    }
    if strays > 0 {
        parts.push(container_count(strays, "stray"));
    }
    if builds > 0 {
        parts.push(container_count(builds, "build"));
    }
    match parts.split_last() {
        // Nothing to name, which still reads as "0 sessions" -- the wording
        // the empty case had before there was anything else to count.
        None => session_count(0),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// `header`, then one line per stray that is skipped rather than removed.
async fn write_stray_skipped<E>(
    stderr: &mut E,
    header: &str,
    strays: &[LabeledContainer],
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if strays.is_empty() {
        return Ok(());
    }
    stderr.write_all(header.as_bytes()).await?;
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
        container_count(strays.len(), "stray")
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

/// Report the working containers whose age could not be established. Not a
/// removal list: without an age there is no telling a month-old stray from a
/// build that started a second ago.
async fn write_build_undatable<E>(stderr: &mut E, undatable: &[BuildContainer]) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    write_build_list(
        stderr,
        "[outrig] skipped build containers with no creation time:\n",
        undatable,
    )
    .await
}

/// Preview every build container that will be removed.
///
/// Everything, never a count: this sweep is not label-scoped, so the only
/// thing standing between it and a container outrig did not make is the
/// reader.
async fn write_build_preview<E>(
    stderr: &mut E,
    builds: &[BuildContainer],
    retention: &str,
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if builds.is_empty() {
        return Ok(());
    }
    let header = format!(
        "[outrig] will remove {} older than {retention} (buildah working containers):\n",
        container_count(builds.len(), "build")
    );
    write_build_list(stderr, &header, builds).await
}

/// `header`, then one line per container. The row shape is written once so
/// the two lists cannot drift into describing the same thing differently.
async fn write_build_list<E>(
    stderr: &mut E,
    header: &str,
    containers: &[BuildContainer],
) -> Result<()>
where
    E: AsyncWrite + Unpin,
{
    if containers.is_empty() {
        return Ok(());
    }
    stderr.write_all(header.as_bytes()).await?;
    for container in containers {
        let line = format!(
            "  {}  {}  image {}\n",
            container.short_id(),
            container.name,
            container.image
        );
        stderr.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

/// `"1 stray container"`, `"2 build containers"`.
fn container_count(count: usize, noun: &str) -> String {
    format!(
        "{count} {noun} {}",
        crate::cli::session_setup::plural(count, "container", "containers")
    )
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

/// `podman rm -f <name>...` for every stray in one invocation, returning the
/// names that are **still there** afterwards -- clean should report a
/// container it could not remove rather than claiming success, and the
/// invocation's own exit status cannot tell it which those are. Callers guard
/// the empty case (`podman rm -f` with no names is an error).
///
/// A failing batch is still an error: that is a removal that did not run, as
/// against one that ran and did not take.
async fn podman_remove_force_batch(names: Vec<String>) -> Result<Vec<String>> {
    engine::remove_batch("podman", &["rm", "-f"], names.clone()).await?;
    let mut left = still_present(&names).await?;
    // A name the batch skipped is not a name podman refuses: asked for it on
    // its own, it removes it at once. So the batch stays the common path and
    // the survivors are retried one at a time, which is also the shape that
    // can attribute a failure -- a batch reports one exit status for every
    // name in it, which is how skipping went unnoticed in the first place.
    //
    // Best-effort: a name that will not go away is reported by the caller
    // rather than failing the whole sweep, because the rest of it did work.
    if !left.is_empty() {
        for name in &left {
            let _ = engine::remove_batch("podman", &["rm", "-f"], vec![name.clone()]).await;
        }
        left = still_present(&left).await?;
    }
    Ok(left)
}

/// Which of `names` podman still has. Reuses the sweep's own listing call
/// rather than adding a second way to read the store -- it is one more `podman
/// ps` and no new parsing *code*, though it does re-parse every row on the
/// host to answer a membership test over `names`. Only the labeled half is
/// consulted: a stray is a container carrying `org.outrig.session`, so a name
/// absent from that list is gone as far as this sweep can ever act on it.
///
/// `plan/next/clean-verify-above-the-seam.md` has the cheaper shape -- podman's
/// own `rm` output already names what it removed -- and why it is not taken
/// here.
async fn still_present(names: &[String]) -> Result<Vec<String>> {
    let (labeled, _running) = list_all_containers().await?;
    let present: BTreeSet<&str> = labeled.iter().map(|c| c.name.as_str()).collect();
    Ok(names
        .iter()
        .filter(|name| present.contains(name.as_str()))
        .cloned()
        .collect())
}

/// `podman ps -a --format json`, decoded into the labeled-container rows (the
/// stray-sweep input) plus the set of *all* running container names (the
/// record-walk running check). Unfiltered so an attach session's borrowed,
/// outrig-unlabeled primary still shows up in the running set. A row missing its
/// name or session label is skipped rather than failing the sweep; one with no
/// readable `Created` is kept, with no creation time.
async fn list_all_containers() -> Result<(Vec<LabeledContainer>, BTreeSet<String>)> {
    let listing =
        engine::capture("podman", &["ps", "-a", "--format", "json"], "outrig clean").await?;
    parse_all_containers(&listing)
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
        labeled.push(LabeledContainer {
            name: name.to_string(),
            session_label: session_label.to_string(),
            sidecar_label,
            running: is_running,
            created: build_containers::created_time(&row),
        });
    }
    Ok((labeled, running))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// A stopped, labeled row for `session`. `created` is the JSON text of its
    /// `Created`, or `None` for a row that has no such field.
    fn stopped_row(session: &str, created: Option<&str>) -> serde_json::Value {
        let mut row = serde_json::json!({
            "Names": [format!("outrig-{session}")],
            "Labels": {"org.outrig.session": session},
            "State": "exited",
        });
        if let Some(created) = created {
            row["Created"] = serde_json::from_str(created).expect("created is JSON");
        }
        row
    }

    fn stray(name: &str, running: bool, created: Option<SystemTime>) -> LabeledContainer {
        LabeledContainer {
            name: name.to_string(),
            session_label: name.to_string(),
            sidecar_label: None,
            running,
            created,
        }
    }

    fn names(containers: &[LabeledContainer]) -> Vec<&str> {
        containers.iter().map(|c| c.name.as_str()).collect()
    }

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
        assert_eq!(labeled[0].created, Some(at(1_700_000_000)));
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

    /// podman writes an unset creation time as Go's zero `time.Time`. Read as
    /// the epoch, that and every other `Created` that is not a time would clear
    /// any cutoff, so each is read as no time at all.
    #[test]
    fn parse_all_containers_reads_a_created_that_is_no_time_as_unknown() {
        for created in [
            None,
            Some("null"),
            Some("-62135596800"),
            Some("0"),
            Some(r#""0001-01-01T00:00:00Z""#),
            Some(r#""yesterday""#),
            Some("1700000000.5"),
        ] {
            let stdout = serde_json::json!([stopped_row("gone", created)]).to_string();
            let (labeled, _) = parse_all_containers(stdout.as_bytes()).expect("parse");
            assert_eq!(labeled.len(), 1, "for {created:?}");
            assert_eq!(labeled[0].created, None, "for {created:?}");
        }
    }

    #[test]
    fn parse_all_containers_reads_an_rfc3339_created() {
        let row = stopped_row("gone", Some(r#""2023-11-14T22:13:20Z""#));
        let stdout = serde_json::json!([row]).to_string();
        let (labeled, _) = parse_all_containers(stdout.as_bytes()).expect("parse");
        assert_eq!(labeled[0].created, Some(at(1_700_000_000)));
    }

    /// The cutoff is what keeps the sweep off a container made a moment ago, so
    /// one whose age cannot be read is reported at every cutoff and removable at
    /// none. #276: it was dated 1970 and removed under any cutoff below 56 years.
    #[test]
    fn classify_strays_reports_a_stray_with_no_creation_time_at_every_cutoff() {
        let now = at(1_800_000_000);
        let day = Duration::from_secs(24 * 60 * 60);
        let labeled = vec![
            stray("undated", false, None),
            stray("undated-running", true, None),
            stray("old", false, Some(now - 40 * day)),
            stray("young", false, Some(now - Duration::from_secs(60))),
        ];
        let cutoffs = [
            (Duration::from_secs(1), vec!["old", "young"]),
            (30 * day, vec!["old"]),
            (3650 * day, vec![]),
            (36500 * day, vec![]),
        ];
        for (cutoff, expected) in cutoffs {
            let (removable, running, undatable) =
                classify_strays(&[], &BTreeSet::new(), &[], labeled.clone(), cutoff, now);
            assert_eq!(names(&removable), expected, "removable at {cutoff:?}");
            assert_eq!(
                names(&running),
                ["undated-running"],
                "running at {cutoff:?}"
            );
            assert_eq!(names(&undatable), ["undated"], "undatable at {cutoff:?}");
        }
    }
}
