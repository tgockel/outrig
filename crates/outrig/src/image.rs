//! Image build via buildah with content-addressed cache.
//!
//! The cache-key helper produces a deterministic 16-hex-char key over
//! `(Dockerfile bytes, resolved build-args, OutRig labels, context content)`.
//! Built images are tagged `<image-config-name>:<key>` (the nameless library
//! path falls back to `outrig-cache:<key>`). [`ensure_image`] probes that tag
//! first; on miss it shells out to `buildah build`. Buildah's own layer cache
//! still helps speed up the build itself when we miss; the project-level tag
//! cache exists so a *hit* skips buildah entirely.
//!
//! A finished build is hashed again, and discarded rather than tagged if the
//! key moved ([`OutrigError::BuildContextChanged`]).

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Write as _};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::config::{
    ImageConfig, ImageSourceRef, McpServerSpec, ResolvedEnvValue, resolve_against,
};
use crate::container::embedded::{self, mcp_config_to_labels, merged_mcp_config_to_labels};
use crate::engine_env;
use crate::error::{IoPathExt, OutrigError, Result};
use crate::process::{self, Cmd, Transcript};
use crate::supervise::{CLEANUP_RETRIES, CleanupGuard, RETRY_BACKOFF, Reissue};

/// Repository used for build-type images that have no image-config name (the
/// nameless library path: [`crate::Outrig::launch`] with a raw build spec).
/// Named call paths use the image-config name as the repository instead, so a
/// built image reads as `<image-config-name>:<hash>` in `podman images`.
const TAG_PREFIX: &str = "outrig-cache";
const KEY_HEX_LEN: usize = 16;
const UNNAMED_IMAGE: &str = "<image>";
const DOCKER_TRANSPORT_PREFIX: &str = "docker://";
const UNSUPPORTED_REMOTE_TRANSPORTS: &[&str] = &[
    "containers-storage:",
    "dir:",
    "docker-archive:",
    "docker-daemon:",
    "oci:",
    "oci-archive:",
];

/// How long a stopped `buildah build` gets to reach its own ending before
/// `SIGKILL` follows.
///
/// A `SIGTERM` reaching buildah mid-`RUN` is forwarded to the command in the
/// stage container and escalated there; the build then fails and unwinds
/// through its own stage cleanup. So the grace has to cover buildah's
/// escalation *plus* the removal of every working container the build made,
/// not merely the time to deliver a signal. Nothing waits on it -- the grace
/// is paid by a detached task, and the drop that started it returns at once
/// -- so erring long costs a process that lingers, and erring short costs the
/// leak this exists to prevent. Measured on buildah 1.33.7, the whole unwind
/// takes about 200 ms.
const BUILD_TERM_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

#[cfg(any(test, feature = "e2e"))]
tokio::task_local! {
    /// Set for the duration of one [`ungraceful_build_termination`] call.
    ///
    /// Task-local rather than a process-wide flag: the override then belongs
    /// to the call being tested instead of to whichever test happens to be
    /// running, and needs no lock, no reset, and no guard against a panicking
    /// test leaving it on.
    static FORCE_UNGRACEFUL_BUILD: ();
}

/// How a cancelled `buildah build` is stopped.
///
/// The only command outrig asks to stop rather than killing outright. Its
/// per-stage working containers carry no mark outrig can put on them and no
/// mark `buildah containers --filter` could select by, so the sole process
/// able to prove which ones belong to this build is buildah, which holds
/// their ids. A `SIGTERM` reaching it during a `RUN` fails the build, and the
/// build unwinds through its own stage cleanup on the way out.
///
/// Not a guarantee, and deliberately not described as one. Outside a `RUN`
/// -- during a pull, a `COPY`, the final commit, or the seam between two
/// `RUN`s -- buildah has no handler registered and dies where it stands, and
/// so does it if outrig's runtime goes away inside the grace. What is left
/// over is what `outrig clean --build-containers` exists to collect.
fn build_termination() -> process::Termination {
    #[cfg(any(test, feature = "e2e"))]
    if FORCE_UNGRACEFUL_BUILD.try_with(|()| ()).is_ok() {
        return process::Termination::Kill;
    }
    process::Termination::TermThenKill(BUILD_TERM_GRACE)
}

/// Run `build` with a cancellation killing buildah outright, as it did before
/// the graceful stop existed.
///
/// Exists so a live-engine test can *show* the working-container leak come
/// back when the stop is taken away, rather than assume the assertion that
/// passes would have failed without it. Compiled only for tests and the `e2e`
/// feature, so it is not part of the shipped surface.
#[cfg(any(test, feature = "e2e"))]
pub async fn ungraceful_build_termination<F: std::future::Future>(build: F) -> F::Output {
    FORCE_UNGRACEFUL_BUILD.scope((), build).await
}

/// Repository name for a build-type image: the image-config name, falling back
/// to [`TAG_PREFIX`] for the nameless library path ([`UNNAMED_IMAGE`]).
fn tag_repo(image: &str) -> &str {
    if image == UNNAMED_IMAGE {
        TAG_PREFIX
    } else {
        image
    }
}

/// A resolved image reference -- either a registry name (`docker.io/x:1`) or a
/// build-type cache tag (`<repo>:<key>`). The wrapped `String` is private so a
/// future tag form, or validation of one, stays a crate-internal change;
/// [`ImageTag::as_str`] and [`Display`](fmt::Display) are the read paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTag(String);

impl ImageTag {
    /// Wrap an already-resolved image reference. Nothing is validated: the
    /// only producers are this module's tag computation and a caller naming an
    /// image podman will resolve.
    pub fn new(tag: impl Into<String>) -> Self {
        Self(tag.into())
    }

    /// The reference as podman and buildah want it on a command line.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the tag for its reference, for callers that hand a `String`
    /// onward rather than borrowing it.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for ImageTag {
    fn from(tag: String) -> Self {
        Self(tag)
    }
}

/// The engine's own content id for one image, as podman reports it: the
/// `sha256:` digest of its config, which names a fixed set of layers and
/// labels for as long as that image exists on the machine.
///
/// A tag is a *pointer*: `registry/x:latest` means whatever the last pull put
/// there, and a pull between a container starting and something asking about
/// its image moves it. An id does not move, so a read addressed by one is a
/// read of the image that is actually running. The field is private and there
/// is no `From<String>`, so the only way to have one is [`image_id`] taking it
/// out of what the engine printed -- without that, "the image" is a `String`
/// like any other and every fallback that produces one, a tag included,
/// type-checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageId(String);

impl ImageId {
    /// The id as podman wants it where it takes an image.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Length of the hex digest in an image id, `sha256:` prefix excluded.
const IMAGE_DIGEST_LEN: usize = 64;

/// The image id in what `podman inspect --format {{.Image}}` printed, if what
/// it printed is one.
///
/// Podman writes the bare 64-character digest; an engine that writes the
/// `sha256:`-prefixed form is read too, since podman accepts either where it
/// takes an image. Anything else -- a tag, a short id, an empty field, an
/// error -- is `None` rather than something to address an image by: the
/// caller's alternative is the tag it is trying not to use, so a lenient read
/// here would put the moving pointer back on the path it was taken off.
pub(crate) fn image_id(printed: &str) -> Option<ImageId> {
    let trimmed = printed.trim();
    let digest = trimmed.strip_prefix("sha256:").unwrap_or(trimmed);
    (digest.len() == IMAGE_DIGEST_LEN
        && digest
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then(|| ImageId(trimmed.to_string()))
}

impl fmt::Display for ImageTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of [`ensure_image`]: the resolved tag plus whether the buildah
/// step was skipped (the tag was already cached). Callers use the flag to
/// drive output -- a cache hit can print a one-line summary, a miss wants
/// the verbose header + buildah stream.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ImageBuildOutcome {
    pub tag: ImageTag,
    pub cache_hit: bool,
}

/// Where a cache-miss `buildah build` or `podman pull` writes its output
/// while it runs. See [`ensure_tagged_image_for_with_output`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildOutput {
    /// Held in memory. A failure carries the last 1 MiB of stderr.
    Captured,
    /// Each line to this process's stderr as `[buildah] <line>` (or
    /// `[podman] <line>` for a pull) as it arrives, so a build that takes
    /// minutes is seen to be running. Once: a [`Transcript`] that already
    /// mirrors to stderr suppresses the live copy. A failure carries no
    /// stderr tail, because the lines are already above it.
    Stderr,
}

pub(crate) struct CacheKey;

impl CacheKey {
    /// Hash `(Dockerfile bytes, sorted build-args, context content)` into a
    /// 16-hex-char blake3 prefix. The build-args must already be resolved to
    /// concrete values. Caller passes absolute paths; `ensure_image` resolves
    /// relative-to-repo-root before calling. Repo-local builds use
    /// [`CacheKey::compute_with_labels`] instead.
    pub(crate) async fn compute(
        dockerfile: &Path,
        build_args: &BTreeMap<String, String>,
        context: &Path,
    ) -> Result<String> {
        let mut hasher = blake3::Hasher::new();

        let dockerfile_bytes = tokio::fs::read(dockerfile)
            .await
            .path_ctx("read", dockerfile)?;
        hasher.update(&dockerfile_bytes);

        hash_pairs(build_args, &mut hasher);

        hash_context(context, &mut hasher).await?;

        let hex = hasher.finalize().to_hex();
        Ok(hex.as_str()[..KEY_HEX_LEN].to_string())
    }

    /// Hash the same build inputs as [`CacheKey::compute`], plus the labels
    /// OutRig will stamp onto repo-local build images. Label changes therefore
    /// produce new tags instead of stale `org.outrig.mcp` metadata.
    pub(crate) async fn compute_with_labels(
        dockerfile: &Path,
        build_args: &BTreeMap<String, String>,
        context: &Path,
        labels: &BTreeMap<String, String>,
    ) -> Result<String> {
        if labels.is_empty() {
            return Self::compute(dockerfile, build_args, context).await;
        }

        let mut hasher = blake3::Hasher::new();

        let dockerfile_bytes = tokio::fs::read(dockerfile)
            .await
            .path_ctx("read", dockerfile)?;
        hasher.update(&dockerfile_bytes);

        hash_pairs(build_args, &mut hasher);
        hasher.update(b"\n[outrig-labels]\n");
        hash_pairs(labels, &mut hasher);

        hash_context(context, &mut hasher).await?;

        let hex = hasher.finalize().to_hex();
        Ok(hex.as_str()[..KEY_HEX_LEN].to_string())
    }
}

/// `pairs` as the lines a cache key hashes: `key=value` and a newline each, in key order.
///
/// A backslash or newline in a key or value is written `\\` or `\n`, so a line holds one pair
/// and a value cannot spell the next. A pair with neither is written as it always was. A key
/// holding `=` needs no escape: buildah splits `--build-arg` at the first one, so `A=1`/`x` and
/// `A`/`1=x` reach it as the same argument.
fn pairs_block(pairs: &BTreeMap<String, String>) -> String {
    let escape = |s: &str| s.replace('\\', r"\\").replace('\n', r"\n");
    let mut block = String::new();
    for (k, v) in pairs {
        let _ = writeln!(block, "{}={}", escape(k), escape(v));
    }
    block
}

/// Hash `pairs` as [`pairs_block`] writes them, after a `0xFF` byte if it escaped anything.
///
/// Escaping writes a newline as a backslash and an `n`, and a backslash as two. Those are the
/// bytes a release before the escaping hashed for a backslash and an `n`, and for two backslashes.
/// The image store outlives an upgrade and a cache probe asks only whether a tag exists, so a key
/// computed from them would reuse the image built for the other value. That release hashed only
/// UTF-8 text here, which never holds `0xFF`, so the byte keeps an escaped block from matching
/// anything it hashed. A backslash gets into the block only as an escape, so a block without one
/// is hashed as it always was, and an image whose pairs hold neither character keeps its tag.
fn hash_pairs(pairs: &BTreeMap<String, String>, hasher: &mut blake3::Hasher) {
    let block = pairs_block(pairs);
    if block.contains('\\') {
        hasher.update(b"\xff");
    }
    hasher.update(block.as_bytes());
}

/// Resolve `build-args` for an image-config. Literal values pass through;
/// `${VAR}` references are read from the host environment and framed with the
/// image name plus the build-arg key on failure. Each keeps its source, so the
/// build passes a reference by name and shows it as `KEY=${VAR}`.
pub(crate) fn resolve_build_args(
    image: &str,
    cfg: &ImageConfig,
) -> Result<BTreeMap<String, ResolvedEnvValue>> {
    let mut resolved = BTreeMap::new();
    for (key, value) in &cfg.build_args {
        let value = ResolvedEnvValue::resolve(value.clone()).map_err(|source| {
            OutrigError::BuildArgResolveFailed {
                image: image.to_string(),
                key: key.clone(),
                source,
            }
        })?;
        resolved.insert(key.clone(), value);
    }
    Ok(resolved)
}

fn repo_build_cache_labels(cfg: &ImageConfig) -> Result<BTreeMap<String, String>> {
    // Sidecar-placed entries run in other containers; they neither belong in
    // this image's label nor affect its cache key.
    mcp_config_to_labels(&embedded::primary_scoped_mcp(&cfg.mcp))
}

/// Compute the deterministic `<repo>:<key>` tag for `cfg` without touching
/// buildah. For image-name configs, the tag is the literal `image-name` value.
/// This resolves `build-args` first because the cache key tracks the concrete
/// values passed to buildah. Useful when a caller wants to print the tag
/// *before* deciding whether to build (e.g. the `outrig build` CLI's verbose
/// header in `doc/usage/build.md`). This nameless variant uses the
/// `outrig-cache` repository; prefer [`compute_tag_for`] when the image-config
/// name is known so the built image is self-describing.
pub async fn compute_tag(cfg: &ImageConfig, repo_root: &Path) -> Result<ImageTag> {
    compute_tag_for(UNNAMED_IMAGE, cfg, repo_root).await
}

/// Named-image variant of [`compute_tag`]. The image-config name becomes the
/// repository of a build-type tag (`<name>:<key>`) so `podman images` shows
/// where the image came from. Also used when config-derived build args may
/// need `${VAR}` resolution so errors can identify the source image-config.
pub async fn compute_tag_for(image: &str, cfg: &ImageConfig, repo_root: &Path) -> Result<ImageTag> {
    match cfg.source() {
        ImageSourceRef::Image { image_name } => Ok(ImageTag::new(image_name)),
        ImageSourceRef::Build { .. } => {
            let build_args = resolve_build_args(image, cfg)?;
            compute_tag_with_build_args(tag_repo(image), cfg, repo_root, &build_args).await
        }
    }
}

async fn compute_tag_with_build_args(
    repo: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
) -> Result<ImageTag> {
    let (dockerfile, context) = cfg.resolved_build_paths(repo_root);
    let labels = repo_build_cache_labels(cfg)?;
    // The key covers the values buildah receives, however each is passed.
    let values = build_args
        .iter()
        .map(|(key, value)| (key.clone(), value.value().to_owned()))
        .collect();
    let key = CacheKey::compute_with_labels(&dockerfile, &values, &context, &labels).await?;
    Ok(ImageTag::new(format!("{repo}:{key}")))
}

/// Refuse to name a finished build `tag` if its inputs no longer hash to it.
///
/// `tag` was computed before buildah started, and buildah reads the context as each `COPY` runs
/// rather than when the build starts. A file saved while an earlier `RUN` was still going is
/// one the image holds and the key does not hash, and under that key the image would be served
/// the next time the old content came back. Called after `buildah build` and before anything
/// is named `tag`, so the temporary image [`into_temp_tag`] removes on the error is all that
/// exists. The same check also trips on an edit after the last `COPY`, which no key can tell
/// from one before it.
async fn ensure_key_still_holds(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
    tag: &ImageTag,
) -> Result<()> {
    let now = compute_tag_with_build_args(tag_repo(image), cfg, repo_root, build_args).await?;
    if now == *tag {
        return Ok(());
    }
    Err(OutrigError::BuildContextChanged {
        image: image.to_string(),
        tag: tag.to_string(),
    })
}

/// Returns `true` iff `tag` already exists in buildah's local image store.
/// `buildah images --quiet <tag>` prints the image id on a hit, and exits 125
/// with `image not known` on a miss; any failure reads as a miss, and so does
/// a success that printed no id.
pub async fn probe_cached(tag: &ImageTag) -> Result<bool> {
    let probe = process::try_capture(
        Cmd::new("buildah")
            .args(["images", "--quiet"])
            .arg(tag.as_str()),
    )
    .await?;
    Ok(probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Logged sibling of [`probe_cached`]. Used by session startup so verbose
/// mode records the cache probe alongside build/start lifecycle commands.
async fn probe_cached_logged(tag: &ImageTag, transcript: Option<&Transcript>) -> Result<bool> {
    let probe = process::try_capture_logged(
        Cmd::new("buildah")
            .args(["images", "--quiet"])
            .arg(tag.as_str()),
        "buildah",
        transcript,
    )
    .await?;
    Ok(probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Returns `true` iff `tag` (an image ref) already exists in podman's local
/// image store. Uses `podman image exists <ref>`.
pub async fn probe_pulled(tag: &ImageTag) -> Result<bool> {
    let probe = process::try_capture(
        Cmd::new("podman")
            .args(["image", "exists"])
            .arg(tag.as_str()),
    )
    .await?;
    Ok(probe.status.success())
}

/// Logged sibling of [`probe_pulled`].
async fn probe_pulled_logged(tag: &ImageTag, transcript: Option<&Transcript>) -> Result<bool> {
    let probe = process::try_capture_logged(
        Cmd::new("podman")
            .args(["image", "exists"])
            .arg(tag.as_str()),
        "podman",
        transcript,
    )
    .await?;
    Ok(probe.status.success())
}

/// Ensure `tag` exists in Podman's local image store without pulling it.
///
/// This is used for raw image references: once a value failed to match an
/// `[images.<name>]` config block, OutRig should behave like `podman run` with
/// `--pull=never` and accept only already-local images. A miss does not say
/// who asked for the ref; [`ensure_local_image_requested_by`] does.
pub async fn ensure_local_image(
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<ImageBuildOutcome> {
    ensure_local_image_with_requester(None, tag, transcript).await
}

/// [`ensure_local_image`] whose miss names `requester` -- where the ref was
/// written, such as `--image` or `[sidecars.tools]` -- so the error points at
/// the setting to look at rather than at one the user may never have used.
pub async fn ensure_local_image_requested_by(
    requester: &str,
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<ImageBuildOutcome> {
    ensure_local_image_with_requester(Some(requester), tag, transcript).await
}

async fn ensure_local_image_with_requester(
    requester: Option<&str>,
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<ImageBuildOutcome> {
    if probe_pulled_logged(tag, transcript).await? {
        tracing::info!(target: "outrig::image", cache_hit = true, "ensured local image {tag}");
        return Ok(ImageBuildOutcome {
            tag: tag.clone(),
            cache_hit: true,
        });
    }
    Err(OutrigError::Configuration(local_image_missing(
        requester, tag,
    )))
}

/// The miss message: what was asked for and by whom, why it was refused, and
/// the command that fixes it. A raw ref is never pulled (see
/// [`ensure_local_image`]), so the fix is the user's to run -- and is meant to
/// be pasted, so the ref is rendered as the one literal argument it is rather
/// than interpolated: a config accepts any nonblank ref, `$(...)` included.
fn local_image_missing(requester: Option<&str>, tag: &ImageTag) -> String {
    let image = tag.as_str();
    let from = requester
        .map(|requester| format!(" (from {requester})"))
        .unwrap_or_default();
    let pull = Cmd::new("podman").arg("pull").arg(image).render();
    format!(
        "image {image:?}{from} is not an [images.<name>] and is not in podman's local image \
         store; outrig does not pull a raw image ref, so run `{pull}` (or build and tag it) \
         first"
    )
}

/// Pull an image by ref via `podman pull`. Stderr is streamed to
/// `tracing::info!` with the `[podman]` prefix.
pub async fn pull_image(tag: &ImageTag) -> Result<()> {
    process::run_streamed_checked(
        Cmd::new("podman").arg("pull").arg(tag.as_str()),
        "podman",
        process::Termination::Kill,
    )
    .await
}

/// Logged sibling of [`pull_image`] for session startup.
async fn pull_image_logged(
    tag: &ImageTag,
    transcript: Option<&Transcript>,
    output: BuildOutput,
) -> Result<()> {
    run_shown_or_captured(
        Cmd::new("podman").arg("pull").arg(tag.as_str()),
        "podman",
        transcript,
        process::Termination::Kill,
        output,
    )
    .await
}

/// The one dispatch on [`BuildOutput`], so the two commands it governs
/// cannot disagree about what each variant means.
async fn run_shown_or_captured(
    cmd: Cmd,
    prefix: &'static str,
    transcript: Option<&Transcript>,
    termination: process::Termination,
    output: BuildOutput,
) -> Result<()> {
    match output {
        BuildOutput::Captured => {
            process::run_capture_logged_terminating(cmd, prefix, transcript, termination, false)
                .await?;
        }
        BuildOutput::Stderr => {
            process::run_capture_shown(cmd, prefix, transcript, termination).await?;
        }
    }
    Ok(())
}

/// Run `buildah build` for `cfg`, tagging the result `tag`, with build-arg
/// resolution errors framed against the selected image-config.
///
/// `tag` is the one [`compute_tag_for`] gave for the same `image`, `cfg` and
/// `repo_root`. A build whose Dockerfile or context no longer hashes to it when
/// buildah finishes is discarded and nothing is tagged; see
/// [`OutrigError::BuildContextChanged`].
pub async fn build_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
) -> Result<()> {
    let build_args = resolve_build_args(image, cfg)?;
    build_image_with_build_args(image, cfg, repo_root, tag, no_cache, &build_args).await
}

/// Run `build` against a temporary tag, and own that tag whatever happens to
/// it.
///
/// The envelope every build path shares, held in one place because its
/// ordering is the load-bearing part: the guard is armed before the tag can
/// exist, so the store never holds it unowned, and released strictly *after*
/// the awaited cleanup -- and only if that worked, per [`discharge`] --
/// because a cancellation landing inside that cleanup is the case the guard is
/// for. Three call sites got that right independently; one is easier to keep
/// right.
///
/// `build` is handed a list for removals it could not finish and that the
/// tag's removal has to wait for: buildah will not remove an image a working
/// container was made from. Those are retried here, awaited and in order, ahead
/// of the tag's own, so they finish before this returns -- a CLI that exits on
/// the build's error would take a background reaper, and the `rmi` queued
/// behind the `rm`, with it. Only what the retries cannot clear is detached.
async fn into_temp_tag<F>(tag: &ImageTag, transcript: Option<&Transcript>, build: F) -> Result<()>
where
    F: AsyncFnOnce(&ImageTag, &mut Vec<Owed>) -> Result<()>,
{
    let temp_tag = temporary_build_tag(tag);
    let temp_owned = temp_tag_guard(&temp_tag);
    let mut first = Vec::new();
    let result = build(&temp_tag, &mut first).await;
    if first.is_empty() {
        discharge(temp_owned, "image not known", transcript).await;
    } else {
        // No lone `rmi` first: it would be refused as in use.
        first.push((temp_owned, "image not known"));
        discharge_in_order(first, transcript).await;
    }
    result
}

async fn build_image_with_build_args(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
) -> Result<()> {
    into_temp_tag(tag, None, async |temp_tag, owed_first| {
        process::run_streamed_checked(
            build_image_cmd(cfg, repo_root, temp_tag, no_cache, build_args),
            "buildah",
            build_termination(),
        )
        .await?;
        ensure_key_still_holds(image, cfg, repo_root, build_args, tag).await?;
        stamp_repo_image_labels(image, temp_tag, tag, &cfg.mcp, no_cache, None, owed_first).await
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn build_image_logged_with_build_args(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
    output: BuildOutput,
) -> Result<()> {
    into_temp_tag(tag, transcript, async |temp_tag, owed_first| {
        run_shown_or_captured(
            build_image_cmd(cfg, repo_root, temp_tag, no_cache, build_args),
            "buildah",
            transcript,
            build_termination(),
            output,
        )
        .await?;
        ensure_key_still_holds(image, cfg, repo_root, build_args, tag).await?;
        stamp_repo_image_labels(
            image, temp_tag, tag, &cfg.mcp, no_cache, transcript, owed_first,
        )
        .await
    })
    .await
}

/// Probe the build tag (`outrig-cache:<key>` for this nameless variant); on
/// miss (or when `no_cache` is set), run `buildah build`. Stderr from buildah
/// is streamed to `tracing::info!` with the `[buildah]` prefix.
pub async fn ensure_image(
    cfg: &ImageConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<ImageBuildOutcome> {
    ensure_image_for(UNNAMED_IMAGE, cfg, repo_root, no_cache).await
}

/// Implementation of [`ensure_image`] with build-arg resolution errors framed
/// against the selected image-config.
///
/// A cache miss's pull or build is captured rather than streamed to this
/// process's own stdout/stderr ([`BuildOutput::Captured`], no transcript):
/// this is the one path [`crate::Outrig::launch`] reaches with
/// [`crate::LaunchSource::Build`], and an embedder may be serving its own
/// protocol over the inherited stdout -- `buildah`'s `STEP 1/3 ...` or an
/// image id landing on it would corrupt that stream. `outrig build`, which
/// owns the terminal, goes through [`pull_image`] / [`build_image_for`]
/// instead and keeps the live stream.
async fn ensure_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<ImageBuildOutcome> {
    match cfg.source() {
        ImageSourceRef::Image { image_name } => {
            let tag = ImageTag::new(image_name);
            if !no_cache && probe_pulled(&tag).await? {
                tracing::info!(target: "outrig::image", cache_hit = true, "ensured image {tag}");
                return Ok(ImageBuildOutcome {
                    tag,
                    cache_hit: true,
                });
            }
            pull_image_logged(&tag, None, BuildOutput::Captured).await?;
            tracing::info!(target: "outrig::image", cache_hit = false, "ensured image {tag}");
            Ok(ImageBuildOutcome {
                tag,
                cache_hit: false,
            })
        }
        ImageSourceRef::Build { .. } => {
            let build_args = resolve_build_args(image, cfg)?;
            let tag =
                compute_tag_with_build_args(tag_repo(image), cfg, repo_root, &build_args).await?;
            if !no_cache && probe_cached(&tag).await? {
                tracing::info!(target: "outrig::image", cache_hit = true, "ensured image {tag}");
                return Ok(ImageBuildOutcome {
                    tag,
                    cache_hit: true,
                });
            }
            build_image_logged_with_build_args(
                image,
                cfg,
                repo_root,
                &tag,
                no_cache,
                None,
                &build_args,
                BuildOutput::Captured,
            )
            .await?;
            tracing::info!(target: "outrig::image", cache_hit = false, "ensured image {tag}");
            Ok(ImageBuildOutcome {
                tag,
                cache_hit: false,
            })
        }
    }
}

/// Ensure an already-computed tag exists, with build-arg resolution errors
/// framed against the selected image-config. This lets session startup
/// write a complete `session.json` and open `logs/container.log` before the
/// buildah probe/build begins, without hashing the Dockerfile/context twice.
///
/// `tag` must be the one [`compute_tag_for`] gave for the same `image`, `cfg`
/// and `repo_root`. A build whose Dockerfile or context changed while it ran
/// is discarded rather than tagged, and fails with
/// [`OutrigError::BuildContextChanged`].
///
/// A cache miss's build or pull is captured; see
/// [`ensure_tagged_image_for_with_output`] to show it as it runs.
pub async fn ensure_tagged_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
) -> Result<ImageBuildOutcome> {
    ensure_tagged_image_for_with_output(
        image,
        cfg,
        repo_root,
        tag,
        no_cache,
        transcript,
        BuildOutput::Captured,
    )
    .await
}

/// [`ensure_tagged_image_for`] with a say in where a cache miss's `buildah
/// build` or `podman pull` writes its output while it runs.
///
/// Only that one command honors `output`. The cache probe, the label commit
/// that follows a build, and the `$ <cmd>` echo of each command are recorded
/// by `transcript` alone, so what [`BuildOutput::Stderr`] adds to a plain
/// session is the build itself and nothing a `--verbose` transcript would
/// not also carry.
pub async fn ensure_tagged_image_for_with_output(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
    output: BuildOutput,
) -> Result<ImageBuildOutcome> {
    match cfg.source() {
        ImageSourceRef::Image { .. } => {
            if !no_cache && probe_pulled_logged(tag, transcript).await? {
                tracing::info!(target: "outrig::image", cache_hit = true, "ensured image {tag}");
                return Ok(ImageBuildOutcome {
                    tag: tag.clone(),
                    cache_hit: true,
                });
            }
            pull_image_logged(tag, transcript, output).await?;
            tracing::info!(target: "outrig::image", cache_hit = false, "ensured image {tag}");
            Ok(ImageBuildOutcome {
                tag: tag.clone(),
                cache_hit: false,
            })
        }
        ImageSourceRef::Build { .. } => {
            let build_args = resolve_build_args(image, cfg)?;
            if !no_cache && probe_cached_logged(tag, transcript).await? {
                tracing::info!(target: "outrig::image", cache_hit = true, "ensured image {tag}");
                return Ok(ImageBuildOutcome {
                    tag: tag.clone(),
                    cache_hit: true,
                });
            }
            build_image_logged_with_build_args(
                image,
                cfg,
                repo_root,
                tag,
                no_cache,
                transcript,
                &build_args,
                output,
            )
            .await?;
            tracing::info!(target: "outrig::image", cache_hit = false, "ensured image {tag}");
            Ok(ImageBuildOutcome {
                tag: tag.clone(),
                cache_hit: false,
            })
        }
    }
}

/// Build a standalone image project with buildah, tagging the result `tag` and
/// stamping `labels` (the project's config, serialized to OCI labels) into the
/// image metadata. `dockerfile` and `context` resolve against `project_dir` by
/// the rule every config path follows, so a leading `~` is the home directory.
/// Stderr from buildah is streamed to `tracing::info!` with the `[buildah]`
/// prefix.
///
/// Unlike [`ensure_image`], there is no content-addressed cache probe: a
/// standalone build tags a stable caller-named ref (e.g. `rust-dev`), not a
/// `<name>:<key>` content-hash tag. `no_cache` therefore only forwards
/// `--no-cache` to buildah.
///
/// The build runs into a temporary tag and is renamed on success, the shape
/// the cached paths already use. `tag` is the caller's requested output: it
/// may exist already, and it may belong to something else, so a cancelled
/// build must not be able to remove it. Only the intermediate image is this
/// build's to clean up, and only that is guarded.
pub async fn build_standalone(
    project_dir: &Path,
    dockerfile: &Path,
    context: &Path,
    tag: &ImageTag,
    no_cache: bool,
    labels: &BTreeMap<String, String>,
) -> Result<()> {
    let dockerfile = resolve_against(project_dir, dockerfile);
    let context = resolve_against(project_dir, context);
    // The cleanup `into_temp_tag` runs below is an *untag* on the success
    // path: `tag_image` has by then given the image a second name, so
    // removing the temporary one leaves the image under the caller's. On the
    // path where the build never reached the tag, it is a real delete --
    // which is what the guard is for.
    into_temp_tag(tag, None, async |temp_tag, _owed_first| {
        // `--label` stays on the build. The cached path commits its labels
        // afterwards because it has to merge the Dockerfile's own against the
        // config's; a standalone project has its whole label set up front.
        process::run_streamed_checked(
            buildah_build_cmd(
                &dockerfile,
                &context,
                temp_tag,
                no_cache,
                &BTreeMap::new(),
                labels,
            ),
            "buildah",
            build_termination(),
        )
        .await?;
        tag_image(temp_tag, tag).await
    })
    .await
}

/// Read the OCI labels off a local image `tag` via `podman image inspect`. This
/// reads image metadata only -- no container runs, and for a local image
/// nothing is pulled. Returns the label map; an image with no labels (podman
/// prints `null`) yields an empty map. A missing tag fails the inspect and
/// surfaces as a process error.
///
/// `transcript`, when present, tees the podman command line and output into the
/// session transcript, matching every other in-session podman call.
pub async fn read_image_labels(
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<BTreeMap<String, String>> {
    read_labels_of(tag.as_str(), transcript).await
}

/// [`read_image_labels`], addressed by the engine's image id instead of a tag.
///
/// A tag is a pointer and a pull moves it, so a label read that a running
/// container's behavior depends on goes through the id of the image that
/// container was started from -- see [`crate::container::Container::image_id`].
pub(crate) async fn read_image_labels_by_id(
    id: &ImageId,
    transcript: Option<&Transcript>,
) -> Result<BTreeMap<String, String>> {
    read_labels_of(id.as_str(), transcript).await
}

/// `podman image inspect <image>` for its labels, where `image` is whatever
/// the caller addresses the image by.
async fn read_labels_of(
    image: &str,
    transcript: Option<&Transcript>,
) -> Result<BTreeMap<String, String>> {
    let cmd = Cmd::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(image)
        .arg("--format")
        .arg("{{json .Config.Labels}}");
    let output = process::run_capture_logged(cmd, "podman", transcript).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(BTreeMap::new());
    }
    serde_json::from_str(trimmed).map_err(|source| {
        OutrigError::Configuration(format!(
            "podman image inspect {image}: invalid labels JSON: {source}"
        ))
    })
}

/// Read a local image's ENTRYPOINT and CMD (OCI `Config.Entrypoint` /
/// `Config.Cmd`) via `podman image inspect`. Either is empty when the image
/// leaves it unset (podman renders the field as `null`). Used to reconstruct
/// the payload command for a `view = "primary"` sidecar, whose real ENTRYPOINT
/// is replaced by the `outrig-enter` launcher.
pub async fn read_image_entrypoint_cmd(
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<(Vec<String>, Vec<String>)> {
    #[derive(serde::Deserialize)]
    struct Config {
        #[serde(rename = "Entrypoint", default)]
        entrypoint: Option<Vec<String>>,
        #[serde(rename = "Cmd", default)]
        cmd: Option<Vec<String>>,
    }
    let cmd = Cmd::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(tag.as_str())
        .arg("--format")
        .arg("{{json .Config}}");
    let output = process::run_capture_logged(cmd, "podman", transcript).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let config: Config = serde_json::from_str(text.trim()).map_err(|source| {
        OutrigError::Configuration(format!(
            "podman image inspect {tag}: invalid config JSON: {source}"
        ))
    })?;
    Ok((
        config.entrypoint.unwrap_or_default(),
        config.cmd.unwrap_or_default(),
    ))
}

/// Read a local image's declared environment (OCI `Config.Env`) via `podman
/// image inspect`, as a `KEY` -> `value` map. This reads image metadata only --
/// no container runs, and for a local image nothing is pulled. An image that
/// declares no environment yields an empty map. A missing tag fails the inspect
/// and surfaces as a process error.
///
/// `transcript`, when present, tees the podman command line and output into the
/// session transcript, matching every other in-session podman call.
pub async fn read_image_env(
    tag: &ImageTag,
    transcript: Option<&Transcript>,
) -> Result<BTreeMap<String, String>> {
    let cmd = Cmd::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(tag.as_str())
        .arg("--format")
        .arg("{{json .Config.Env}}");
    let output = process::run_capture_logged(cmd, "podman", transcript).await?;
    parse_env_json(tag, &String::from_utf8_lossy(&output.stdout))
}

/// Split podman's `Config.Env` array into a map.
///
/// An entry without `=` is skipped rather than guessed at -- it names neither a
/// key nor a value. The split is on the *first* `=`, so a value that contains
/// one survives (`FOO=a=b` is `FOO` -> `a=b`), and a repeated key takes the
/// last entry, which is the order a runtime applies the list in anyway.
fn parse_env_json(tag: &ImageTag, text: &str) -> Result<BTreeMap<String, String>> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "null" {
        return Ok(BTreeMap::new());
    }
    let entries: Vec<String> = serde_json::from_str(trimmed).map_err(|source| {
        OutrigError::Configuration(format!(
            "podman image inspect {tag}: invalid env JSON: {source}"
        ))
    })?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect())
}

/// Read OCI labels from a registry ref via `skopeo inspect` without pulling
/// image layers. Plain image refs are inspected as `docker://<ref>`, and an
/// already-prefixed `docker://...` ref is accepted. Other explicit skopeo
/// transports are rejected so `outrig image inspect --remote` stays a registry
/// read rather than a generic skopeo wrapper.
pub async fn read_remote_image_labels(image_ref: &str) -> Result<BTreeMap<String, String>> {
    read_remote_image_labels_with_program("skopeo", image_ref).await
}

async fn read_remote_image_labels_with_program(
    program: &'static str,
    image_ref: &str,
) -> Result<BTreeMap<String, String>> {
    let remote_ref = docker_transport_ref(image_ref)?;
    let output = match process::run_capture(
        Cmd::new(program)
            .arg("inspect")
            .arg("--no-tags")
            .arg(&remote_ref),
    )
    .await
    {
        Ok(output) => output,
        Err(OutrigError::Spawn { source, .. }) if source.kind() == ErrorKind::NotFound => {
            return Err(OutrigError::Configuration(
                "`outrig image inspect --remote` requires `skopeo` on PATH".to_string(),
            ));
        }
        Err(err) => return Err(err),
    };
    labels_from_skopeo_inspect(&remote_ref, &output.stdout)
}

fn docker_transport_ref(image_ref: &str) -> Result<String> {
    let image_ref = image_ref.trim();
    if image_ref.is_empty() {
        return Err(OutrigError::Configuration(
            "remote image ref must not be empty".to_string(),
        ));
    }
    if image_ref == DOCKER_TRANSPORT_PREFIX {
        return Err(OutrigError::Configuration(
            "remote image ref must include a repository after docker://".to_string(),
        ));
    }
    if image_ref.starts_with(DOCKER_TRANSPORT_PREFIX) {
        return Ok(image_ref.to_string());
    }
    if image_ref.contains("://") || has_unsupported_remote_transport(image_ref) {
        return Err(OutrigError::Configuration(format!(
            "unsupported remote image ref {image_ref:?}; use a registry ref or docker://<ref>"
        )));
    }
    Ok(format!("{DOCKER_TRANSPORT_PREFIX}{image_ref}"))
}

fn has_unsupported_remote_transport(image_ref: &str) -> bool {
    UNSUPPORTED_REMOTE_TRANSPORTS
        .iter()
        .filter_map(|transport| image_ref.strip_prefix(transport))
        .any(|rest| rest.starts_with('/') || rest.starts_with('.') || rest.contains(':'))
}

fn labels_from_skopeo_inspect(remote_ref: &str, stdout: &[u8]) -> Result<BTreeMap<String, String>> {
    let parsed: SkopeoInspect = serde_json::from_slice(stdout).map_err(|source| {
        OutrigError::Configuration(format!(
            "skopeo inspect {remote_ref}: invalid inspect JSON: {source}"
        ))
    })?;
    Ok(parsed.labels.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct SkopeoInspect {
    #[serde(rename = "Labels")]
    labels: Option<BTreeMap<String, String>>,
}

/// Arm the removal of a build's temporary tag.
///
/// The [`discharge`] after each build is what runs on every path that reaches
/// it; this covers the paths on which that does not remove the tag. A dropped
/// future never gets there, since the cleanup is a statement after an
/// `.await`, and buildah can refuse a removal it does reach. The tag is named
/// before `buildah build` runs, so arming it first leaves no instant at which
/// the store could hold it unowned.
fn temp_tag_guard(temp_tag: &ImageTag) -> CleanupGuard {
    // The tag carries this build's pid and nonce, so it names nothing another
    // build could create -- a retry is safe.
    CleanupGuard::arm(
        Cmd::new("buildah").arg("rmi").arg(temp_tag.as_str()),
        Reissue::Safe,
    )
}

fn temporary_build_tag(final_tag: &ImageTag) -> ImageTag {
    let raw = final_tag.as_str();
    // Only a `:` after the last `/` introduces a tag. The cached path always
    // hands this a `<repo>:<key>`, but `build_standalone` hands it whatever
    // ref the project asked for -- and `localhost:5000/team/img` split on the
    // last colon yields a "tag" of `5000/team/img`, which no engine accepts.
    // A ref with no tag at all keeps its repository rather than falling back
    // to `outrig-cache`, so the temporary image reads as the caller's.
    let (repo, key) = match raw.rsplit_once(':') {
        Some((repo, key)) if !key.contains('/') => (repo, key),
        _ => (raw, "image"),
    };
    let repo = if repo.is_empty() { TAG_PREFIX } else { repo };
    // The caller's key is carried only so the temporary image is
    // recognizable, so it is bounded rather than trusted: an OCI tag may be
    // 128 characters, and the cached path's 16-hex key always fits while a
    // standalone project's ref is whatever it wants to be. Adding a nonce to
    // a valid 128-character tag would push the *build* over the limit and
    // fail it outright -- a build that used to succeed.
    let key = truncate_chars(key, TEMP_TAG_KEY_LIMIT);
    ImageTag::new(format!(
        "{repo}:outrig-tmp-{}-{:032x}-{key}",
        std::process::id(),
        temp_nonce()
    ))
}

/// How much of the caller's tag a temporary tag echoes back.
///
/// The rest of the name is bounded by construction: `outrig-tmp-` plus a pid,
/// a 32-hex nonce and two separators is at most 55 characters, so this keeps
/// the whole tag inside the 128 an engine accepts.
const TEMP_TAG_KEY_LIMIT: usize = 32;

/// The first `limit` characters of `text`, split on a character boundary so a
/// multi-byte ref cannot be cut in half.
fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

/// Give a built image the name its caller asked for.
///
/// `buildah tag` adds a name to an image that already exists; no layer is
/// copied and the image id is unchanged. Removing the temporary name
/// afterwards is therefore an untag, not a delete -- which is exactly what
/// makes the build's [`CleanupGuard`] safe to leave armed across it.
async fn tag_image(source: &ImageTag, target: &ImageTag) -> Result<()> {
    run_buildah_capture(
        Cmd::new("buildah")
            .arg("tag")
            .arg(source.as_str())
            .arg(target.as_str()),
        // `build_standalone` is the only caller and has no transcript to
        // thread: `outrig image build` streams buildah's output rather than
        // capturing it into a session log.
        None,
    )
    .await
}

fn temporary_builder_name() -> String {
    format!("outrig-label-{}-{}", std::process::id(), temp_nonce())
}

/// A value no other build can share.
///
/// Random rather than a timestamp. `SystemTime::now()` is not distinct per
/// call -- two builds reaching it inside one clock tick get the same reading,
/// and NTP can step it backwards -- and `std::process::id()` does not separate
/// them either, since concurrent builds in one process share a pid. These
/// names are what a *detached* cleanup selects by, sometimes seconds after the
/// build that owned them ended, and a shared one would have each build
/// removing the other's temporary tag or working container. That is also what
/// lets them be `Reissue::Safe`: a selector nothing else can become is one a
/// retry cannot misdirect. Same 128 bits, and the same reasoning, as
/// `container::attempt_token`.
fn temp_nonce() -> u128 {
    use rand::Rng;

    let mut buf = [0_u8; 16];
    rand::rng().fill_bytes(&mut buf);
    u128::from_be_bytes(buf)
}

/// Merge `source_tag`'s own `org.outrig.mcp` label with `config_mcp` and commit
/// the result as `final_tag`.
///
/// `image` is the image-config's name, and it -- not `source_tag` -- is what a
/// label that will not decode is reported against. `source_tag` is this
/// build's temporary tag, which [`into_temp_tag`] removes before the caller
/// sees the error.
async fn stamp_repo_image_labels(
    image: &str,
    source_tag: &ImageTag,
    final_tag: &ImageTag,
    config_mcp: &BTreeMap<String, McpServerSpec>,
    no_cache: bool,
    transcript: Option<&Transcript>,
    owed_first: &mut Vec<Owed>,
) -> Result<()> {
    let inherited = read_image_labels(source_tag, transcript).await?;
    let labels = merged_mcp_config_to_labels(image, &inherited, config_mcp)?;
    commit_image_with_labels(
        source_tag, final_tag, &labels, no_cache, transcript, owed_first,
    )
    .await
}

async fn commit_image_with_labels(
    source_tag: &ImageTag,
    final_tag: &ImageTag,
    labels: &BTreeMap<String, String>,
    no_cache: bool,
    transcript: Option<&Transcript>,
    owed_first: &mut Vec<Owed>,
) -> Result<()> {
    let builder = temporary_builder_name();
    // `outrig-label-<pid>-<nonce>`, so likewise this build's alone.
    let builder_owned =
        CleanupGuard::arm(Cmd::new("buildah").arg("rm").arg(&builder), Reissue::Safe);
    // Whether this call committed `final_tag` itself (`true`) or found another
    // session had already won the race for it (`false`): the two leave the
    // working container in different states, so `result` alone cannot steer
    // the cleanup below.
    let result = async {
        run_buildah_capture(
            Cmd::new("buildah")
                .arg("from")
                .arg("--pull=never")
                .arg("--name")
                .arg(&builder)
                .arg(source_tag.as_str()),
            transcript,
        )
        .await?;

        let mut config = Cmd::new("buildah").arg("config");
        for (key, value) in labels {
            config = config.arg("--label").arg(format!("{key}={value}"));
        }
        config = config.arg(&builder);
        run_buildah_capture(config, transcript).await?;

        // Issue #393: two sessions that miss the content-addressed cache for
        // the same tag at once both reach this commit, and `buildah commit`
        // over an existing tag re-points it rather than refusing -- so the
        // loser would overwrite the winner's image with its own
        // (same-content, later-metadata) copy, leaving the winner's former
        // image dangling with nothing left pointing at it. Re-probing `final_tag`
        // immediately before committing to it cannot close the race outright
        // (the two commits can still interleave), but it does close the far
        // more common case: one build finished first and is already sitting
        // there to be seen. Losing this probe means skipping the commit, not
        // failing the build -- the caller asked for an image at `final_tag`,
        // and one is there.
        //
        // A forced rebuild (`no_cache`) skips the probe and always commits: the
        // caller asked to replace whatever is at `final_tag`, stale copied
        // gitignored files and all, and treating another session's unrelated
        // win as this one's own would silently keep it stale. `no_cache`
        // callers are rarer and already pay for a full rebuild, so losing the
        // ordinary race's protection here costs them only the same
        // re-point-over-an-existing-tag outcome `buildah commit` always had.
        if !no_cache && probe_cached_logged(final_tag, transcript).await? {
            return Ok(false);
        }

        run_buildah_capture(
            Cmd::new("buildah")
                .arg("commit")
                .arg("--rm")
                .arg("--quiet")
                .arg(&builder)
                .arg(final_tag.as_str()),
            transcript,
        )
        .await?;
        Ok(true)
    }
    .await;

    // `commit --rm` takes the working container on the success path. On
    // every other path -- the race lost above, or a buildah failure -- it is
    // still there, and the awaited removal reaches it. One buildah refuses is
    // handed to the caller still armed rather than reissued here: the
    // caller's `rmi` of the image this container was made from is refused as
    // in use until the container is gone, so the two are detached as one
    // chain, `rm` first, and the `rmi` gets its whole retry budget after the
    // `rm` is done.
    match result {
        Ok(true) => builder_owned.release(),
        Ok(false) | Err(_) => owed_first.extend(
            try_remove(builder_owned, "container not known", transcript)
                .await
                .map(|(guard, _)| (guard, "container not known")),
        ),
    }
    result.map(|_| ())
}

async fn run_buildah_capture(cmd: Cmd, transcript: Option<&Transcript>) -> Result<()> {
    if transcript.is_some() {
        process::run_capture_logged(cmd, "buildah", transcript).await?;
    } else {
        process::run_capture(cmd).await?;
    }
    Ok(())
}

/// Run `guard`'s removal now, awaited, and release the guard only if that
/// left nothing behind; `absent` is the removal's stderr for a target that was
/// never there.
///
/// Otherwise the guard drops armed, and its drop reissues the removal. The
/// failure is warned about rather than returned: the caller is holding the
/// build's own result, which a cleanup failure must not replace.
async fn discharge(guard: CleanupGuard, absent: &str, transcript: Option<&Transcript>) {
    if let Some((guard, failure)) = try_remove(guard, absent, transcript).await {
        warn_left_behind(&failure);
        drop(guard);
    }
}

/// A removal that is owed, and the stderr [`left_nothing`] reads as its target
/// already being gone.
type Owed = (CleanupGuard, &'static str);

/// Run each removal in `owed` awaited, in order, retrying a refused one on the
/// schedule [`supervise`](crate::supervise) gives a detached cleanup, and only
/// then move on to the next.
///
/// What still will not go after its retries is detached, still in order, so a
/// later one is not raced against an earlier one that is still owed.
async fn discharge_in_order(owed: Vec<Owed>, transcript: Option<&Transcript>) {
    let mut left = Vec::new();
    for (guard, absent) in owed {
        let mut guard = Some(guard);
        let mut backoff = RETRY_BACKOFF;
        for attempt in 0..=CLEANUP_RETRIES {
            let Some(owed) = guard.take() else { break };
            match try_remove(owed, absent, transcript).await {
                None => {}
                Some((still_owed, failure)) => {
                    if attempt == CLEANUP_RETRIES {
                        warn_left_behind(&failure);
                    } else {
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                    }
                    guard = Some(still_owed);
                }
            }
        }
        left.extend(guard);
    }
    CleanupGuard::detach_chain(left);
}

fn warn_left_behind(failure: &OutrigError) {
    tracing::warn!(
        target: "outrig::image",
        "a build cleanup left its target behind; reissuing it in the background: {failure}"
    );
}

/// One awaited attempt at `guard`'s removal: `None` once it left nothing
/// behind, otherwise the guard still armed, with why it did.
async fn try_remove(
    guard: CleanupGuard,
    absent: &str,
    transcript: Option<&Transcript>,
) -> Option<(CleanupGuard, OutrigError)> {
    let cmd = guard.removal().clone();
    let failure = match process::try_capture_logged(cmd.clone(), "buildah", transcript).await {
        Ok(output) if left_nothing(&output, absent) => {
            guard.release();
            return None;
        }
        Ok(output) => cmd.process_error(
            output.status.code(),
            process::tail_string(&output.stderr, process::STDERR_TAIL_LIMIT),
        ),
        Err(e) => e,
    };
    Some((guard, failure))
}

/// Whether a removal's output shows its target gone: removed now, or never
/// there.
///
/// buildah exits 125 for a target that does not exist, the same code as for
/// every other failure, so the storage layer's "not known" sentinel in stderr
/// is the only thing that tells the two apart (measured against buildah
/// 1.33.7). A presence probe would not avoid the string match: `buildah images
/// --quiet` answers a miss the same way.
///
/// Absence has to count. A build that fails before it creates its tag reaches
/// the `rmi` of that tag every time, and reading the answer as a failure would
/// spend a round of detached retries on nothing. Should a later buildah reword
/// the sentinel, that round is what it costs -- never a leak.
fn left_nothing(output: &std::process::Output, absent: &str) -> bool {
    output.status.success() || String::from_utf8_lossy(&output.stderr).contains(absent)
}

fn build_image_cmd(
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
) -> Cmd {
    let (dockerfile, context) = cfg.resolved_build_paths(repo_root);
    buildah_build_cmd(
        &dockerfile,
        &context,
        tag,
        no_cache,
        build_args,
        &BTreeMap::new(),
    )
}

/// Assemble a `buildah build --tag <tag> --file <dockerfile> [--no-cache]
/// [--build-arg ...] [--label ...] <context>` command. A build-arg resolved
/// from a `${VAR}` reference is passed by name; see [`engine_env`]. `dockerfile` and
/// `context` are already resolved, through [`resolve_against`]. Used directly
/// by standalone image builds; repo-local builds first build a temporary image,
/// then stamp merged OutRig labels in a final metadata-only commit.
///
/// Labels stamp a standalone image's config (e.g. `org.outrig.mcp`) into OCI
/// metadata. `Cmd` builds argv directly (no shell), so a JSON label value --
/// braces, quotes, spaces -- reaches buildah as one literal argument, and
/// buildah splits `key=value` on the first `=` (JSON has none at the top
/// level).
fn buildah_build_cmd(
    dockerfile: &Path,
    context: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, ResolvedEnvValue>,
    labels: &BTreeMap<String, String>,
) -> Cmd {
    let mut cmd = Cmd::new("buildah")
        .arg("build")
        .arg("--tag")
        .arg(tag.as_str())
        .arg("--file")
        .arg(dockerfile);
    if no_cache {
        cmd = cmd.arg("--no-cache");
    }
    for (k, v) in build_args {
        cmd = engine_env::push_keyed(cmd, "--build-arg", k, v.value(), Some(v.source()));
    }
    for (k, v) in labels {
        cmd = cmd.arg("--label").arg(format!("{k}={v}"));
    }
    cmd.arg(context)
}

/// Key the build context `ctx`: by [`hash_git_context`] in a git worktree, or
/// by [`hash_non_git_context`], the same in-process walk, outside one.
async fn hash_context(ctx: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    if is_git_context(ctx).await? {
        hash_git_context(ctx, hasher).await
    } else {
        hash_non_git_context(ctx, hasher).await
    }
}

async fn is_git_context(ctx: &Path) -> Result<bool> {
    let output = process::try_capture(
        Cmd::new("git")
            .arg("-C")
            .arg(ctx)
            .args(["rev-parse", "--git-dir"]),
    )
    .await?;
    Ok(output.status.success())
}

/// Whether git takes `dir` as the top of a worktree of its own. `rev-parse
/// --show-prefix` prints an empty prefix there; from a directory an enclosing
/// repository reaches into, it prints that directory's path.
async fn is_worktree_root(dir: &Path) -> Result<bool> {
    let output = process::try_capture(
        Cmd::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", "--show-prefix"]),
    )
    .await?;
    Ok(output.status.success() && output.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Hash every file in a git-hosted `ctx` that no ignore rule excludes, tracked
/// or not, each by its context-relative path, `st_mode`, and content, and every
/// directory no rule excludes, empty ones included, by path and `st_mode`.
///
/// buildah is handed the whole directory, so that is the set a `COPY` can
/// reach, less what `.gitignore` excludes: build output (`target/`,
/// `node_modules/`) stays out so it cannot bust the cache. The mode carries
/// both the file type and the permission bits `COPY` preserves. Directories
/// come from [`context_dirs`], not git's listing.
///
/// Where the rules exclude every file -- the context sits under an ignored
/// directory, as `.agents/` kept out of version control would, or every file
/// in it is ignored -- they say nothing about which are build inputs, and the
/// whole tree counts. A file tracked although a rule matches it, force-added
/// or committed before the rule, is excluded all the same.
async fn hash_git_context(ctx: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    hash_git_tree(ctx, Path::new(""), hasher).await
}

/// [`hash_git_context`] for `ctx/dir`, a directory git lists from its own
/// repository: the context itself, or a nested repository in it. Paths are
/// keyed relative to `ctx/dir`, but symlinks resolve against `ctx`, as buildah
/// resolves every `COPY` source.
async fn hash_git_tree(ctx: &Path, dir: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    let tree = ctx.join(dir);
    // What the rules admit, tracked or not, and what is tracked although a
    // rule matches it.
    let (paths, excluded) = tokio::try_join!(
        ls_files(&tree, &["--cached", "--others", "--exclude-standard"]),
        ls_files(&tree, &["--cached", "--ignored", "--exclude-standard"]),
    )?;

    // One blocking pass rather than an async round-trip per stat and read:
    // a context can be a whole checkout.
    let (root, base) = (ctx.to_path_buf(), dir.to_path_buf());
    let entries = tokio::task::spawn_blocking(move || {
        let mut tree = TreeHasher::new(&root);
        let mut entries = Vec::with_capacity(paths.len());
        for rel in paths {
            if let Some((mode, entry)) = tree.listed(&base.join(OsStr::from_bytes(&rel)))? {
                entries.push((rel, mode, entry));
            }
        }
        Ok::<_, OutrigError>(entries)
    })
    .await
    .expect("context hashing task panicked")?;

    // Nothing left, or only what a rule excludes: the whole tree counts, and
    // so does its own mode, which `TreeHasher::dir` leaves to a parent.
    if entries
        .iter()
        .all(|(rel, ..)| excluded.binary_search(rel).is_ok())
    {
        let root = tokio::fs::symlink_metadata(&tree)
            .await
            .path_ctx("stat", &tree)?;
        update_record(hasher, b"", root.mode(), &hash_whole_tree(ctx, dir).await?);
        return Ok(());
    }

    // Asked only here: from a directory below an ignored one, which the
    // whole-tree case above covers, git exits 128.
    let ignored = ls_files(
        &tree,
        &["--others", "--ignored", "--exclude-standard", "--directory"],
    )
    .await?;
    // A nested repository is walked by its own call, under its own rules.
    let skip: HashSet<Vec<u8>> = ignored
        .iter()
        .filter_map(|rel| rel.strip_suffix(b"/"))
        .chain(
            entries
                .iter()
                .filter(|(.., entry)| matches!(entry, ContextEntry::Dir))
                .map(|(rel, ..)| rel.strip_suffix(b"/").unwrap_or(rel)),
        )
        .map(<[u8]>::to_vec)
        .collect();
    let dirs = tokio::task::spawn_blocking(move || context_dirs(&tree, &skip))
        .await
        .expect("context hashing task panicked")?;

    for (rel, mode, entry) in entries {
        let digest = match entry {
            ContextEntry::Leaf(digest) => digest,
            ContextEntry::Dir => {
                let sub_dir = dir.join(OsStr::from_bytes(&rel));
                // A nested repository or initialized submodule is hashed by
                // the same rule, in its own repository. Anything else -- an
                // uninitialized submodule, or one whose `.git` git rejects --
                // is hashed as a plain tree: git answers there from the
                // enclosing repository and lists the directory as `./`, so
                // recursing would never end.
                if is_worktree_root(&ctx.join(&sub_dir)).await? {
                    let mut sub = blake3::Hasher::new();
                    Box::pin(hash_git_tree(ctx, &sub_dir, &mut sub)).await?;
                    sub.finalize()
                } else {
                    hash_whole_tree(ctx, &sub_dir).await?
                }
            }
        };
        update_record(hasher, &rel, mode, &digest);
    }
    for (rel, mode) in dirs {
        update_record(hasher, &rel, mode, &NO_CONTENT);
    }
    Ok(())
}

/// One entry of the context stream. Everything after the NUL-terminated path is
/// fixed-width, so two different listings cannot feed the same stream.
fn update_record(hasher: &mut blake3::Hasher, rel: &[u8], mode: u32, digest: &blake3::Hash) {
    hasher.update(rel);
    hasher.update(b"\0");
    hasher.update(&mode.to_le_bytes());
    hasher.update(digest.as_bytes());
}

/// The digest of an entry with no content to read: a directory, FIFO, socket,
/// or device.
const NO_CONTENT: blake3::Hash = blake3::Hash::from_bytes([0; 32]);

/// Every directory under `tree`, itself included, that no ignore rule
/// excludes, as its path relative to `tree` (empty for `tree`) and its
/// `st_mode`. Empty ones count: buildah copies them, and git, which tracks
/// files, lists neither them nor any directory's mode.
///
/// `skip` holds the paths, relative to `tree`, that are not walked: directories
/// the rules exclude, and those a nested repository or submodule owns. `.git`
/// and symlinks are not walked either.
fn context_dirs(tree: &Path, skip: &HashSet<Vec<u8>>) -> Result<Vec<(Vec<u8>, u32)>> {
    let mut dirs = Vec::new();
    // Next to visit last, so names come out sorted.
    let mut pending = vec![Vec::new()];
    while let Some(rel) = pending.pop() {
        let path = tree.join(OsStr::from_bytes(&rel));
        // A directory under one the build user cannot search has no mode to
        // read; its parent's record is all of it that shows.
        let Some(meta) = unless_denied(std::fs::symlink_metadata(&path), "stat", &path)? else {
            continue;
        };
        dirs.push((rel.clone(), meta.mode()));

        // Keyed but not opened, as `git ls-files` does.
        let Some(entries) = unless_denied(std::fs::read_dir(&path), "read", &path)? else {
            continue;
        };
        let mut children = Vec::new();
        for entry in entries {
            let entry = entry.path_ctx("read", &path)?;
            let Some(file_type) = unless_denied(entry.file_type(), "stat", &path)? else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            let mut child = rel.clone();
            if !child.is_empty() {
                child.push(b'/');
            }
            child.extend_from_slice(name.as_bytes());
            if !skip.contains(&child) {
                children.push(child);
            }
        }
        children.sort_unstable();
        pending.extend(children.into_iter().rev());
    }
    Ok(dirs)
}

/// `None` for what the build user may not look at. git warns and carries on
/// there, and a root-owned volume in a context must not fail a build that
/// worked.
fn unless_denied<T>(
    result: std::io::Result<T>,
    op: &'static str,
    path: &Path,
) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => Ok(None),
        Err(err) => Err(err).path_ctx(op, path),
    }
}

/// What `git ls-files -z <flags> .` lists in `tree`, sorted and deduplicated.
/// Without `--full-name`, paths come out relative to `tree`, as buildah sees
/// them.
async fn ls_files(tree: &Path, flags: &[&str]) -> Result<Vec<Vec<u8>>> {
    let listing = process::run_capture(
        Cmd::new("git")
            .arg("-C")
            .arg(tree)
            .args(["ls-files", "-z"])
            .args(flags)
            .arg("."),
    )
    .await?;

    // `--cached` and `--others` come out as two runs, each sorted; pin one
    // order. An unmerged path is listed once per stage.
    let mut paths: Vec<Vec<u8>> = listing
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// `ctx/dir` whole, ignored files and all, by [`TreeHasher::dir`]. From the
/// context's own root (an empty `dir`) the walk is [`TreeHasher::whole`].
async fn hash_whole_tree(ctx: &Path, dir: &Path) -> Result<blake3::Hash> {
    let (root, dir) = (ctx.to_path_buf(), dir.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let mut tree = if dir.as_os_str().is_empty() {
            TreeHasher::whole(&root)
        } else {
            TreeHasher::new(&root)
        };
        tree.dir(&dir)
    })
    .await
    .expect("context hashing task panicked")
}

/// What one listed path contributes to [`hash_git_context`].
enum ContextEntry {
    /// Anything but a directory, hashed in the blocking pass.
    Leaf(blake3::Hash),
    /// A directory git lists as one entry: a submodule or nested repository.
    Dir,
}

/// Hashes paths under the build context directly, off git's listing: what a
/// symlink resolves to, a directory git lists whole with no repository of its
/// own, and a tree git's ignore rules exclude whole.
///
/// `COPY <link>` copies a link's referent, which git may never list (ignored,
/// or under `.git`) and which may hold more links, so every link is followed
/// wherever it is met. A referent is hashed once; a link back into one still
/// being hashed is a loop, and counts by its text alone.
///
/// A walk of the whole context, [`Self::whole`], follows no link: it meets the
/// referent in its own place anyway, and following into it from every link
/// that points at an ancestor would hash a subtree again for each.
struct TreeHasher<'a> {
    ctx: &'a Path,
    /// Mode and digest of each referent hashed so far, by context-relative
    /// path.
    done: HashMap<PathBuf, (u32, blake3::Hash)>,
    /// Referents being hashed now.
    active: HashSet<PathBuf>,
    /// Set for a walk of the whole context: the first path each file with more
    /// than one link was met at, by device and inode. buildah keeps a `COPY
    /// .`'s hard links, so a later link to a file is keyed as such, not by
    /// bytes a pair of copies would share. Only the one walk, in name order,
    /// makes "first" mean something; a referent hashed from the middle of
    /// another walk would not.
    whole: Option<HashMap<(u64, u64), PathBuf>>,
}

impl<'a> TreeHasher<'a> {
    fn new(ctx: &'a Path) -> Self {
        Self {
            ctx,
            done: HashMap::new(),
            active: HashSet::new(),
            whole: None,
        }
    }

    /// A hasher for [`Self::dir`] of the context's own root: every path in `ctx`
    /// is met once, in name order.
    fn whole(ctx: &'a Path) -> Self {
        Self {
            whole: Some(HashMap::new()),
            ..Self::new(ctx)
        }
    }

    /// Stat and hash one path git listed. `None` for a path the worktree no
    /// longer has: a tracked file deleted without `git rm`, which buildah does
    /// not see either. A directory is left to the caller, which asks git
    /// whether it is a repository of its own.
    fn listed(&mut self, rel: &Path) -> Result<Option<(u32, ContextEntry)>> {
        let path = self.ctx.join(rel);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).path_ctx("stat", path),
        };
        let entry = if meta.is_dir() {
            ContextEntry::Dir
        } else {
            ContextEntry::Leaf(self.entry(rel, &meta)?)
        };
        Ok(Some((meta.mode(), entry)))
    }

    /// `ctx/rel` by what it is: a file by its content, a symlink by
    /// [`Self::link`], a directory by [`Self::dir`], and a FIFO, socket, or
    /// device by nothing -- its mode says which, and opening a FIFO would
    /// block.
    fn entry(&mut self, rel: &Path, meta: &std::fs::Metadata) -> Result<blake3::Hash> {
        let file_type = meta.file_type();
        if file_type.is_file() {
            match self.earlier_link(rel, meta) {
                Some(first) => Ok(hard_link_digest(&first)),
                None => hash_file(&self.ctx.join(rel)),
            }
        } else if file_type.is_symlink() {
            self.link(rel)
        } else if file_type.is_dir() {
            self.dir(rel)
        } else {
            Ok(NO_CONTENT)
        }
    }

    /// Where a whole-context walk first met the regular file `meta` describes,
    /// when `rel` is a later link to it. A file whose other links are outside
    /// the context, or that has none, has no earlier one: nothing `COPY` sees
    /// tells it from a plain file.
    fn earlier_link(&mut self, rel: &Path, meta: &std::fs::Metadata) -> Option<PathBuf> {
        let first_links = self.whole.as_mut()?;
        if meta.nlink() < 2 {
            return None;
        }
        match first_links.entry((meta.dev(), meta.ino())) {
            Entry::Occupied(first) => Some(first.get().clone()),
            Entry::Vacant(slot) => {
                slot.insert(rel.to_path_buf());
                None
            }
        }
    }

    /// Each entry of the directory `ctx/rel`, by name, mode, and digest.
    fn dir(&mut self, rel: &Path) -> Result<blake3::Hash> {
        let path = self.ctx.join(rel);
        let mut names = std::fs::read_dir(&path)
            .and_then(|entries| {
                entries
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<Vec<_>>>()
            })
            .path_ctx("read", &path)?;
        names.sort_unstable();

        let mut hasher = blake3::Hasher::new();
        for name in names {
            let rel = rel.join(&name);
            let path = self.ctx.join(&rel);
            let meta = std::fs::symlink_metadata(&path).path_ctx("stat", &path)?;
            let digest = self.entry(&rel, &meta)?;
            update_record(&mut hasher, name.as_bytes(), meta.mode(), &digest);
        }
        Ok(hasher.finalize())
    }

    /// The symlink `ctx/rel`: its text, as `COPY .` keeps it, then the mode
    /// and digest of what `COPY <link>` copies in its place, if anything.
    fn link(&mut self, rel: &Path) -> Result<blake3::Hash> {
        let path = self.ctx.join(rel);
        let target = std::fs::read_link(&path).path_ctx("read", &path)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(target.as_os_str().as_bytes());
        hasher.update(b"\0");
        if self.whole.is_none()
            && let Some(referent) = resolve_in_context(self.ctx, rel)
            && let Some((mode, digest)) = self.referent(referent)?
        {
            hasher.update(&mode.to_le_bytes());
            hasher.update(digest.as_bytes());
        }
        Ok(hasher.finalize())
    }

    /// Mode and digest of `ctx/rel`, which a link resolved to. `None` when
    /// nothing is there, or when it is still being hashed -- a loop.
    fn referent(&mut self, rel: PathBuf) -> Result<Option<(u32, blake3::Hash)>> {
        if let Some(done) = self.done.get(&rel) {
            return Ok(Some(*done));
        }
        let Ok(meta) = std::fs::symlink_metadata(self.ctx.join(&rel)) else {
            return Ok(None);
        };
        if !self.active.insert(rel.clone()) {
            return Ok(None);
        }
        let digest = self.entry(&rel, &meta);
        self.active.remove(&rel);
        let done = (meta.mode(), digest?);
        self.done.insert(rel, done);
        Ok(Some(done))
    }
}

/// What a later hard link to a file contributes in place of its bytes: the
/// context-relative path of the first link, which the key already hashes the
/// bytes of. The derive-key domain keeps it from ever matching a file's
/// content hash, whatever the file holds.
fn hard_link_digest(first: &Path) -> blake3::Hash {
    blake3::Hasher::new_derive_key("outrig build context hard link")
        .update(first.as_os_str().as_bytes())
        .finalize()
}

fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let file = std::fs::File::open(path).path_ctx("open", path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(file).path_ctx("read", path)?;
    Ok(hasher.finalize())
}

/// Resolve the symlink at `ctx/rel` to a path relative to `ctx`, as buildah
/// resolves a `COPY` source: with `ctx` as `/`, so an absolute target or a
/// `..` past the top stays inside. A target -- a relative one joined to the
/// link's directory first -- is cleaned before any of it is resolved, so
/// `alias/..` cancels even where `alias` is itself a link, and a component
/// that is not there is kept as written. `None` past buildah's limit on links
/// followed -- a loop.
fn resolve_in_context(ctx: &Path, rel: &Path) -> Option<PathBuf> {
    // buildah's maxLoopsFollowed.
    const MAX_LINKS: usize = 64;
    // Cleaned paths hold only names, so the walk below never meets `..`.
    let names = |path: &Path| -> Vec<OsString> {
        clean_lexically(path)
            .components()
            .rev()
            .map(|c| c.as_os_str().to_owned())
            .collect()
    };

    // Still to walk, next name last.
    let mut pending = names(rel);
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(name) = pending.pop() {
        let next = resolved.join(&name);
        let Ok(target) = std::fs::read_link(ctx.join(&next)) else {
            resolved = next;
            continue;
        };
        links += 1;
        if links > MAX_LINKS {
            return None;
        }
        // Joining an absolute target replaces `resolved` outright.
        pending.extend(names(&resolved.join(target)));
        resolved = PathBuf::new();
    }
    Some(resolved)
}

/// `path`'s names alone, each `..` cancelling the one before it and none
/// climbing above the start. A leading `/` is dropped: the caller's root is
/// the context.
fn clean_lexically(path: &Path) -> PathBuf {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                cleaned.pop();
            }
            Component::Normal(name) => cleaned.push(name),
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    cleaned
}

/// Key a context that is not in a git repository: every file and directory in
/// it, by [`TreeHasher::whole`], since there is no ignore rule to narrow the
/// set and no listing but the filesystem's own. A symlink counts by its text
/// and a hard link by the first link to the same file, as the `tar` stream this
/// replaced had them. Runs in process rather than shelling out to `tar`, so
/// building from a non-git context no longer needs `tar` on the host at all,
/// GNU or otherwise -- the prior `--sort=name` flag needed GNU tar 1.28+, which
/// a minimal distro image's busybox `tar` does not implement -- and a read
/// failure now comes back as the `OutrigError::Path` that named it, not a bare
/// `tar` exit code with nothing in its stderr.
async fn hash_non_git_context(ctx: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    let root = tokio::fs::symlink_metadata(ctx)
        .await
        .path_ctx("stat", ctx)?;
    update_record(
        hasher,
        b"",
        root.mode(),
        &hash_whole_tree(ctx, Path::new("")).await?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::config::EnvValue;

    /// An image id is taken only from output that is one. Anything else --
    /// a tag, a short id, an error podman printed -- is `None`, because the
    /// caller's only alternative is the tag whose moving is the whole reason
    /// the id is being asked for. Accepting a token would put that pointer
    /// back on the path, under a type that says it cannot be there. The same
    /// argument the container's `engine_id` makes about a container selector.
    #[test]
    fn an_image_id_is_taken_only_from_output_that_is_one() {
        let digest = "c1a1b4b8ff0d48b4e2ee1dd5c6cf5f0e0ac0bd0a7a8a4d1c5b7e8f90a1b2c3d4";
        assert_eq!(digest.len(), IMAGE_DIGEST_LEN);
        // What podman's `{{.Image}}` prints, and the prefixed spelling podman
        // also accepts where it takes an image.
        assert_eq!(image_id(digest).as_ref().map(ImageId::as_str), Some(digest));
        let prefixed = format!("sha256:{digest}");
        assert_eq!(
            image_id(&format!("{prefixed}\n"))
                .as_ref()
                .map(ImageId::as_str),
            Some(prefixed.as_str()),
            "the prefix is kept: it is part of what podman is handed"
        );

        for not_an_id in [
            // Nothing at all: a container that is gone inspects to this.
            String::new(),
            "   \n".to_string(),
            // A tag. This is the one the fallback would have reached for.
            "registry.example/x:latest".to_string(),
            "<none>".to_string(),
            // A short id, which podman would resolve -- and which is why it is
            // not accepted as an identity here.
            digest[..12].to_string(),
            // Right length, wrong alphabet, and the uppercase spelling of a
            // digest that is written lowercase everywhere podman prints one.
            "z".repeat(IMAGE_DIGEST_LEN),
            digest.to_uppercase(),
            // Right shape, but two of them.
            format!("{digest} {digest}"),
        ] {
            assert_eq!(image_id(&not_an_id), None, "not an id: {not_an_id:?}");
        }
    }

    /// The issue #355 shape: a sidecar's raw ref that is not local was blamed
    /// on `--image`, and the error never named the command that fixes it.
    #[test]
    fn a_missing_local_image_names_its_requester_and_the_pull() {
        let tag = ImageTag::new("ghcr.io/example/mcp-tools:1");

        let message = local_image_missing(Some("[sidecars.tools]"), &tag);
        assert!(
            message.starts_with(
                "image \"ghcr.io/example/mcp-tools:1\" (from [sidecars.tools]) is not an \
                 [images.<name>]"
            ),
            "{message}"
        );
        assert!(
            message.contains("`podman pull ghcr.io/example/mcp-tools:1`"),
            "{message}"
        );
        assert!(!message.contains("--image"), "{message}");

        let anonymous = local_image_missing(None, &tag);
        assert!(!anonymous.contains("(from"), "{anonymous}");
        assert!(anonymous.contains("`podman pull "), "{anonymous}");
    }

    /// The pull advice is copied into a shell, and a ref is any nonblank
    /// string: one with a command substitution or an apostrophe must stay a
    /// single quoted argument, not code.
    #[test]
    fn the_pull_advice_keeps_a_hostile_ref_one_literal_argument() {
        let substitution =
            local_image_missing(None, &ImageTag::new("missing$(printf marker >&2):1"));
        assert!(
            substitution.contains("`podman pull 'missing$(printf marker >&2):1'`"),
            "{substitution}"
        );

        let apostrophe = local_image_missing(None, &ImageTag::new("it's:1"));
        assert!(
            apostrophe.contains("`podman pull 'it'\\''s:1'`"),
            "{apostrophe}"
        );

        let plain = local_image_missing(None, &ImageTag::new("ghcr.io/example/mcp-tools:1"));
        assert!(
            plain.contains("`podman pull ghcr.io/example/mcp-tools:1`"),
            "a ref of safe bytes stays bare: {plain}"
        );
    }

    /// The issue #324 shape: a token in a build-arg must not be on buildah's
    /// command line, where `ps` reads it, nor in anything a failure prints.
    #[test]
    fn build_image_cmd_passes_a_referenced_build_arg_by_name() {
        let cfg = ImageConfig::from_dockerfile("Dockerfile", ".");
        let resolved = BTreeMap::from([
            (
                "GH_TOKEN".to_string(),
                ResolvedEnvValue::assume(
                    EnvValue::EnvRef("GITHUB_TOKEN".to_string()),
                    "secret-token",
                ),
            ),
            (
                "NODE_VERSION".to_string(),
                ResolvedEnvValue::assume(EnvValue::Literal("20".to_string()), "20"),
            ),
        ]);

        let cmd = build_image_cmd(
            &cfg,
            Path::new("/repo"),
            &ImageTag::new("outrig-cache:test"),
            false,
            &resolved,
        );

        let argv = cmd.exec_args();
        for expected in ["--build-arg", "GH_TOKEN", "NODE_VERSION=20"] {
            assert!(
                argv.contains(&OsString::from(expected)),
                "{expected}: {argv:?}"
            );
        }
        assert!(
            argv.iter()
                .all(|arg| !arg.to_string_lossy().contains("secret-token")),
            "the value is not on the command line: {argv:?}"
        );
        assert_eq!(
            cmd.hidden_env(),
            [("GH_TOKEN".to_string(), "secret-token".to_string())]
        );
        let rendered = cmd.render();
        assert!(
            rendered.contains("'GH_TOKEN=${GITHUB_TOKEN}'"),
            "{rendered}"
        );
        assert!(rendered.contains("NODE_VERSION=20"), "{rendered}");
        assert!(!rendered.contains("secret-token"), "{rendered}");
    }

    #[test]
    fn docker_transport_ref_accepts_plain_and_docker_refs() {
        assert_eq!(
            docker_transport_ref("quay.io/acme/rust-dev:latest").expect("plain ref"),
            "docker://quay.io/acme/rust-dev:latest"
        );
        assert_eq!(
            docker_transport_ref("docker://quay.io/acme/rust-dev@sha256:abc").expect("docker ref"),
            "docker://quay.io/acme/rust-dev@sha256:abc"
        );
        assert_eq!(
            docker_transport_ref("localhost:5000/acme/rust-dev:latest").expect("port ref"),
            "docker://localhost:5000/acme/rust-dev:latest"
        );
        assert_eq!(
            docker_transport_ref("oci:latest").expect("short image tag"),
            "docker://oci:latest"
        );
    }

    #[test]
    fn docker_transport_ref_rejects_empty_and_non_registry_refs() {
        for image_ref in [
            "",
            "   ",
            "docker://",
            "oci:/tmp/layout:latest",
            "dir:/tmp/image",
            "docker-daemon:busybox:latest",
            "http://registry.example.com/image:latest",
        ] {
            let err = docker_transport_ref(image_ref).expect_err("ref should be rejected");
            assert!(matches!(err, OutrigError::Configuration(_)));
        }
    }

    #[test]
    fn labels_from_skopeo_inspect_reads_labels_map() {
        let labels = labels_from_skopeo_inspect(
            "docker://example.com/acme/rust-dev:latest",
            br#"{
                "Name": "example.com/acme/rust-dev",
                "Labels": {
                    "org.opencontainers.image.description": "Rust tooling",
                    "org.outrig.mcp": "{\"fs\":[\"mcp-server-filesystem\",\"/workspace\"]}"
                }
            }"#,
        )
        .expect("labels parse");

        assert_eq!(
            labels["org.opencontainers.image.description"],
            "Rust tooling"
        );
        assert_eq!(
            labels["org.outrig.mcp"],
            r#"{"fs":["mcp-server-filesystem","/workspace"]}"#
        );
    }

    #[test]
    fn labels_from_skopeo_inspect_allows_null_or_missing_labels() {
        assert!(
            labels_from_skopeo_inspect("docker://example.com/plain:latest", br#"{"Labels":null}"#)
                .expect("null labels parse")
                .is_empty()
        );
        assert!(
            labels_from_skopeo_inspect("docker://example.com/plain:latest", br#"{"Name":"plain"}"#)
                .expect("missing labels parse")
                .is_empty()
        );
    }

    #[test]
    fn labels_from_skopeo_inspect_rejects_invalid_json() {
        let err =
            labels_from_skopeo_inspect("docker://example.com/plain:latest", br#"{"Labels": "#)
                .expect_err("invalid JSON must fail");

        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(
            err.to_string().contains("invalid inspect JSON"),
            "got: {err}"
        );
    }

    #[test]
    fn parse_env_json_reads_key_value_entries() {
        let env = parse_env_json(
            &ImageTag::new("outrig-cache:test"),
            r#"["PATH=/usr/local/bin:/usr/bin","RUSTFLAGS=--cfg=tokio_unstable","EMPTY="]"#,
        )
        .expect("env parse");

        assert_eq!(env["PATH"], "/usr/local/bin:/usr/bin");
        // Split on the first `=` only: the value keeps its own.
        assert_eq!(env["RUSTFLAGS"], "--cfg=tokio_unstable");
        assert_eq!(env["EMPTY"], "");
        assert_eq!(env.len(), 3);
    }

    #[test]
    fn parse_env_json_skips_an_entry_with_no_equals() {
        let env = parse_env_json(
            &ImageTag::new("outrig-cache:test"),
            r#"["PATH=/usr/bin","BARE_NAME"]"#,
        )
        .expect("env parse");

        assert_eq!(env["PATH"], "/usr/bin");
        assert!(!env.contains_key("BARE_NAME"), "got: {env:?}");
        assert_eq!(env.len(), 1);
    }

    #[test]
    fn parse_env_json_takes_the_last_of_a_repeated_key() {
        let env = parse_env_json(
            &ImageTag::new("outrig-cache:test"),
            r#"["PATH=/first","PATH=/second"]"#,
        )
        .expect("env parse");

        assert_eq!(env["PATH"], "/second");
    }

    #[test]
    fn parse_env_json_allows_null_or_empty_output() {
        let tag = ImageTag::new("outrig-cache:test");

        assert!(
            parse_env_json(&tag, "null\n")
                .expect("null env parse")
                .is_empty()
        );
        assert!(
            parse_env_json(&tag, "[]")
                .expect("empty env parse")
                .is_empty()
        );
        assert!(
            parse_env_json(&tag, "")
                .expect("empty output parse")
                .is_empty()
        );
    }

    #[test]
    fn parse_env_json_rejects_invalid_json() {
        let err = parse_env_json(&ImageTag::new("outrig-cache:test"), r#"["PATH=/usr/bin""#)
            .expect_err("invalid JSON must fail");

        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(err.to_string().contains("invalid env JSON"), "got: {err}");
        assert!(err.to_string().contains("outrig-cache:test"), "got: {err}");
    }

    /// A removal discharges its guard only by working or by finding nothing
    /// there. The absent answers are buildah 1.33.7's own text, exit 125 and
    /// all -- the code every other failure exits with too.
    #[test]
    fn a_removal_is_discharged_only_by_success_or_a_known_absence() {
        use std::os::unix::process::ExitStatusExt;

        let output = |code: i32, stderr: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        };

        assert!(left_nothing(&output(0, ""), "image not known"));
        assert!(left_nothing(
            &output(
                125,
                "Error: 1 error occurred:\n\t* repo:outrig-tmp-1-ab-key: image not known\n\n"
            ),
            "image not known"
        ));
        assert!(left_nothing(
            &output(
                125,
                "Error: removing container \"outrig-label-1-2\": container not known\n"
            ),
            "container not known"
        ));

        // Refusals that leave the target where it was.
        assert!(!left_nothing(
            &output(
                125,
                "Error: 1 error occurred:\n\t* image used by 5e1f: image is in use by a container\n"
            ),
            "image not known"
        ));
        assert!(!left_nothing(
            &output(
                125,
                "Error: removing outrig-label-1-2: database is locked\n"
            ),
            "container not known"
        ));
        // Each removal is read against its own sentinel only.
        assert!(!left_nothing(
            &output(125, "Error: container not known\n"),
            "image not known"
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_remote_image_labels_invokes_skopeo_inspect_no_tags() {
        let program = fake_skopeo(
            "if [ \"$1\" != inspect ] || [ \"$2\" != --no-tags ] || \
             [ \"$3\" != docker://example.com/acme/rust-dev:latest ]; then\n\
             echo bad argv: \"$@\" >&2\n\
             exit 42\n\
             fi\n\
             printf '%s' '{\"Labels\":{\"org.outrig.tags\":\"[\\\"remote\\\"]\"}}'\n",
        );

        let labels = read_labels_via_fake(program, "example.com/acme/rust-dev:latest")
            .await
            .expect("fake skopeo succeeds");

        assert_eq!(labels["org.outrig.tags"], r#"["remote"]"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_remote_image_labels_surfaces_registry_failures() {
        let program = fake_skopeo("echo registry auth required >&2\nexit 7\n");

        let err = read_labels_via_fake(program, "example.com/private:latest")
            .await
            .expect_err("fake skopeo failure must surface");

        let OutrigError::Process {
            exit_code,
            stderr_tail,
            ..
        } = err
        else {
            panic!("expected process error");
        };
        assert_eq!(exit_code, Some(7));
        assert!(stderr_tail.contains("registry auth required"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_remote_image_labels_reports_missing_skopeo() {
        let err = read_remote_image_labels_with_program(
            "__outrig_missing_skopeo_for_test__",
            "example.com/acme/rust-dev:latest",
        )
        .await
        .expect_err("missing binary must fail");

        assert!(matches!(err, OutrigError::Configuration(_)));
        assert!(err.to_string().contains("requires `skopeo`"), "got: {err}");
    }

    /// Run [`read_remote_image_labels_with_program`] against a `fake_skopeo`
    /// script, retrying on a transient `ETXTBSY` ("text file busy").
    ///
    /// Writing an executable and immediately exec-ing it races with other test
    /// threads: while we hold the freshly written file's write fd, an unrelated
    /// `Command` spawn elsewhere in the process can `fork()` and briefly inherit
    /// that fd, so our `execve` sees the file as still open for write. The
    /// condition clears in microseconds, so a short bounded retry is robust.
    /// (Production never writes the program it execs, so it cannot hit this.)
    async fn read_labels_via_fake(
        program: &'static str,
        image_ref: &str,
    ) -> Result<BTreeMap<String, String>> {
        for _ in 0..100 {
            match read_remote_image_labels_with_program(program, image_ref).await {
                Err(OutrigError::Spawn { source, .. })
                    if source.kind() == ErrorKind::ExecutableFileBusy =>
                {
                    // Yield so the racing thread can finish its exec and close the
                    // inherited write fd, clearing the busy file.
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                other => return other,
            }
        }
        panic!("fake skopeo stayed ETXTBSY across 100 attempts");
    }

    fn fake_skopeo(body: &str) -> &'static str {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("skopeo");
        let script = format!("#!/bin/sh\n{body}");
        std::fs::write(&path, script).expect("write fake skopeo");
        let mut permissions = std::fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).expect("chmod fake skopeo");
        let path = path.to_str().expect("utf-8 fake skopeo path").to_string();
        std::mem::forget(dir);
        Box::leak(path.into_boxed_str())
    }
}

#[cfg(test)]
#[path = "image_cache_tests.rs"]
mod image_cache_tests;

#[cfg(test)]
#[path = "build_args_env_value_tests.rs"]
mod build_args_env_value_tests;
