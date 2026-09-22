//! The buildah working containers an interrupted build can leave behind.
//!
//! Third and last of `outrig clean`'s sweeps, and the only one that is
//! opt-in. The other two remove containers outrig can prove are its own: they
//! carry `org.outrig.session`, a label outrig put there. Nothing equivalent
//! exists here. buildah names a stage working container after the image it
//! came from, offers no way to label or name it, and
//! `buildah containers --filter` selects only on id, name, and ancestor -- so
//! a sweep of these is a sweep of *every* buildah working container on the
//! machine, including one a person made by hand with `buildah from`.
//!
//! That is why it is behind a flag, why it previews everything it intends to
//! remove, and why it will not run without an age. The cutoff is the only
//! thing standing between this and a build that is running right now.
//!
//! Two listings, joined on id, because neither engine answers the whole
//! question: `buildah containers --json` is authoritative about which
//! containers are working containers and what their ids are, and
//! `podman ps -a --external` is the one that reports when they were created.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, SystemTime};

use crate::cli::engine;
use crate::error::{OutrigError, Result};

/// Named in the "needs X on PATH" message, so a reader learns which sweep
/// asked for the engine rather than only which engine was missing.
const WANTED_BY: &str = "outrig clean --build-containers";

/// One buildah working container, as the two listings together describe it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildContainer {
    pub id: String,
    pub name: String,
    pub image: String,
    /// `None` when no external row carried a creation time for this id. Such
    /// a container is reported and left alone: an unknown age cannot clear a
    /// cutoff, and the container it belongs to may be seconds old.
    pub created: Option<SystemTime>,
}

impl BuildContainer {
    /// The short id engines print, which is what a preview line should show.
    pub fn short_id(&self) -> &str {
        let len = self.id.len().min(12);
        &self.id[..len]
    }
}

/// Both listings, joined. Called only when the flag is set, so with it off
/// neither engine is asked anything and `clean` still needs only podman.
///
/// Concurrent because the two are independent read-only listings and neither
/// consumes the other: serializing them would pay two engine startups end to
/// end for no ordering the join needs.
pub async fn list_build_containers() -> Result<Vec<BuildContainer>> {
    let (working, external) = tokio::try_join!(
        engine::capture("buildah", &["containers", "--json"], WANTED_BY),
        engine::capture(
            "podman",
            &["ps", "-a", "--external", "--format", "json"],
            WANTED_BY
        ),
    )?;
    join_build_containers(&working, &external)
}

/// Join the authoritative listing with the one that knows creation times.
///
/// Pure, so the shapes both engines emit can be pinned from fixtures without
/// either installed. Rows present only in podman's `--external` listing are
/// ignored: buildah decides what counts as a working container.
pub fn join_build_containers(working: &[u8], external: &[u8]) -> Result<Vec<BuildContainer>> {
    // `buildah containers --json` prints `null`, not `[]`, for an empty
    // store.
    let rows: Option<Vec<serde_json::Value>> = serde_json::from_slice(working).map_err(|e| {
        OutrigError::Configuration(format!("buildah containers --json: invalid JSON: {e}"))
    })?;
    let rows: Vec<serde_json::Value> = rows.unwrap_or_default();
    // Bounded by the working containers, not by everything podman can see:
    // the external listing is every container on the machine, and all but a
    // handful of its rows are about to be thrown away.
    let wanted: BTreeSet<&str> = rows
        .iter()
        .filter_map(|row| row.get("id").and_then(|v| v.as_str()))
        .collect();
    let created = external_creation_times(external, &wanted)?;
    let mut containers = Vec::new();
    for row in &rows {
        let Some(id) = row.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        containers.push(BuildContainer {
            id: id.to_string(),
            name: row
                .get("containername")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            image: row
                .get("imagename")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            created: created.get(id).copied(),
        });
    }
    Ok(containers)
}

/// Creation times out of `podman ps -a --external --format json`, keyed by id.
///
/// `Created` is seconds since the epoch on some podman versions and an
/// RFC 3339 string on others. Anything else yields no entry, which the
/// classifier reads as "unknown age" rather than "old enough".
fn external_creation_times(
    external: &[u8],
    wanted: &BTreeSet<&str>,
) -> Result<BTreeMap<String, SystemTime>> {
    let rows: Option<Vec<serde_json::Value>> = serde_json::from_slice(external).map_err(|e| {
        OutrigError::Configuration(format!("podman ps --external: invalid JSON: {e}"))
    })?;
    let mut times = BTreeMap::new();
    for row in rows.unwrap_or_default() {
        let Some(id) = row.get("Id").and_then(|v| v.as_str()).filter(|id| wanted.contains(id))
        else {
            continue;
        };
        let Some(created) = row.get("Created") else {
            continue;
        };
        let at = created
            .as_i64()
            .and_then(epoch_seconds)
            .or_else(|| created.as_str().and_then(parse_rfc3339));
        if let Some(at) = at {
            times.insert(id.to_string(), at);
        }
    }
    Ok(times)
}

/// A creation time from epoch seconds, or `None` if it does not describe one.
///
/// podman serializes an unset creation time as Go's zero `time.Time`, which
/// is `-62135596800` seconds -- and clamping that to zero would turn "no idea
/// when this was made" into "made in 1970", which clears every cutoff. That
/// is precisely the container this sweep must not remove, so a value at or
/// before the epoch is read as no value at all.
fn epoch_seconds(secs: i64) -> Option<SystemTime> {
    let secs = u64::try_from(secs).ok().filter(|secs| *secs > 0)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// The crate's one answer to "RFC 3339 string to `SystemTime`", reused so
/// there is not a second one that rounds differently.
///
/// Same rule as [`epoch_seconds`]: Go's zero time renders as
/// `0001-01-01T00:00:00Z`, and a pre-epoch stamp is an unset one.
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let at = crate::session::iso_systime::from_iso(text).ok()?;
    (at > SystemTime::UNIX_EPOCH).then_some(at)
}

/// Split into `(removable, undatable)`.
///
/// A container with no resolvable creation time is never removable. The
/// cutoff is the only thing separating a stray from a build in flight, so a
/// container that cannot be measured against it is reported instead.
pub fn classify_build_containers(
    containers: Vec<BuildContainer>,
    older_than: Duration,
    now: SystemTime,
) -> (Vec<BuildContainer>, Vec<BuildContainer>) {
    let mut removable = Vec::new();
    let mut undatable = Vec::new();
    for container in containers {
        match container.created {
            Some(created)
                if now
                    .duration_since(created)
                    .is_ok_and(|age| age >= older_than) =>
            {
                removable.push(container)
            }
            Some(_) => {}
            None => undatable.push(container),
        }
    }
    (removable, undatable)
}

/// `buildah rm <id>...` in one invocation, propagating failure.
///
/// By id: the decision to remove was reached from a name and an age, but the
/// removal itself names the one identity nothing else can hold. Callers guard
/// the empty case, which `buildah rm` rejects.
pub async fn buildah_remove_batch(ids: Vec<String>) -> Result<()> {
    engine::remove_batch("buildah", &["rm"], ids).await
}

#[cfg(test)]
#[path = "build_containers_tests.rs"]
mod build_containers_tests;
