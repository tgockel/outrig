//! Image build via buildah with content-addressed cache.
//!
//! The cache-key helper produces a deterministic 16-hex-char key over
//! `(Dockerfile bytes, resolved build-args, OutRig labels, context content)`.
//! Built images are tagged `<image-config-name>:<key>` (the nameless library
//! path falls back to `outrig-cache:<key>`). [`ensure_image`] probes that tag
//! first; on miss it shells out to `buildah build`. Buildah's own layer cache
//! still helps speed up the build itself when we miss; the project-level tag
//! cache exists so a *hit* skips buildah entirely.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::ErrorKind;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{ImageConfig, ImageSourceRef, McpServerSpec};
use crate::container::embedded::{mcp_config_to_labels, merged_mcp_config_to_labels};
use crate::error::{OutrigError, Result};
use crate::process::{self, Cmd, Transcript};

/// Repository used for build-type images that have no image-config name (the
/// nameless library path: [`crate::Outrig::launch`] with a raw build spec).
/// Named call paths use the image-config name as the repository instead, so a
/// built image reads as `<image-config-name>:<hash>` in `podman images`.
const TAG_PREFIX: &str = "outrig-cache";
const KEY_HEX_LEN: usize = 16;
const TAR_READ_CHUNK: usize = 64 * 1024;
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

/// Repository name for a build-type image: the image-config name, falling back
/// to [`TAG_PREFIX`] for the nameless library path ([`UNNAMED_IMAGE`]).
fn tag_repo(image: &str) -> &str {
    if image == UNNAMED_IMAGE {
        TAG_PREFIX
    } else {
        image
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTag(pub String);

impl fmt::Display for ImageTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Outcome of [`ensure_image`]: the resolved tag plus whether the buildah
/// step was skipped (the tag was already cached). Callers use the flag to
/// drive output -- a cache hit can print a one-line summary, a miss wants
/// the verbose header + buildah stream.
#[derive(Debug, Clone)]
pub struct ImageBuildOutcome {
    pub tag: ImageTag,
    pub cache_hit: bool,
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

        let dockerfile_bytes = tokio::fs::read(dockerfile).await?;
        hasher.update(&dockerfile_bytes);

        let mut block = String::new();
        for (k, v) in build_args {
            let _ = writeln!(block, "{k}={v}");
        }
        hasher.update(block.as_bytes());

        if is_git_context(context).await? {
            hash_git_context(context, &mut hasher).await?;
        } else {
            hash_tar_context(context, &mut hasher).await?;
        }

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

        let dockerfile_bytes = tokio::fs::read(dockerfile).await?;
        hasher.update(&dockerfile_bytes);

        let mut block = String::new();
        for (k, v) in build_args {
            let _ = writeln!(block, "{k}={v}");
        }
        hasher.update(block.as_bytes());

        let mut block = String::new();
        block.push_str("\n[outrig-labels]\n");
        for (k, v) in labels {
            let _ = writeln!(block, "{k}={v}");
        }
        hasher.update(block.as_bytes());

        if is_git_context(context).await? {
            hash_git_context(context, &mut hasher).await?;
        } else {
            hash_tar_context(context, &mut hasher).await?;
        }

        let hex = hasher.finalize().to_hex();
        Ok(hex.as_str()[..KEY_HEX_LEN].to_string())
    }
}

/// Resolve `build-args` for an image-config. Literal values pass through;
/// `${VAR}` references are read from the host environment and framed with the
/// image name plus the build-arg key on failure.
pub(crate) fn resolve_build_args(
    image: &str,
    cfg: &ImageConfig,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (key, value) in &cfg.build_args {
        let value = value
            .resolve()
            .map_err(|source| OutrigError::BuildArgResolveFailed {
                image: image.to_string(),
                key: key.clone(),
                source,
            })?;
        resolved.insert(key.clone(), value);
    }
    Ok(resolved)
}

fn repo_build_cache_labels(cfg: &ImageConfig) -> Result<BTreeMap<String, String>> {
    mcp_config_to_labels(&cfg.mcp)
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
        ImageSourceRef::Image { image_name } => Ok(ImageTag(image_name.to_string())),
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
    build_args: &BTreeMap<String, String>,
) -> Result<ImageTag> {
    let dockerfile = repo_root.join(cfg.dockerfile.as_ref().expect("build path validated"));
    let context = repo_root.join(cfg.context.as_ref().expect("build path validated"));
    let labels = repo_build_cache_labels(cfg)?;
    let key = CacheKey::compute_with_labels(&dockerfile, build_args, &context, &labels).await?;
    Ok(ImageTag(format!("{repo}:{key}")))
}

/// Returns `true` iff `tag` already exists in buildah's local image store.
/// `buildah images --quiet <tag>` prints the image id on a hit and nothing
/// on a miss; either way exits 0, so we ignore the status and inspect
/// stdout.
pub async fn probe_cached(tag: &ImageTag) -> Result<bool> {
    let probe =
        process::try_capture(Cmd::new("buildah").args(["images", "--quiet"]).arg(&tag.0)).await?;
    Ok(probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Logged sibling of [`probe_cached`]. Used by session startup so verbose
/// mode records the cache probe alongside build/start lifecycle commands.
async fn probe_cached_logged(tag: &ImageTag, transcript: Option<&Transcript>) -> Result<bool> {
    let probe = process::try_capture_logged(
        Cmd::new("buildah").args(["images", "--quiet"]).arg(&tag.0),
        "buildah",
        transcript,
    )
    .await?;
    Ok(probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace))
}

/// Returns `true` iff `tag` (an image ref) already exists in podman's local
/// image store. Uses `podman image exists <ref>`.
pub async fn probe_pulled(tag: &ImageTag) -> Result<bool> {
    let probe =
        process::try_capture(Cmd::new("podman").args(["image", "exists"]).arg(&tag.0)).await?;
    Ok(probe.status.success())
}

/// Logged sibling of [`probe_pulled`].
async fn probe_pulled_logged(tag: &ImageTag, transcript: Option<&Transcript>) -> Result<bool> {
    let probe = process::try_capture_logged(
        Cmd::new("podman").args(["image", "exists"]).arg(&tag.0),
        "podman",
        transcript,
    )
    .await?;
    Ok(probe.status.success())
}

/// Ensure `tag` exists in Podman's local image store without pulling it.
///
/// This is used for raw CLI image references: once a value failed to match an
/// `[images.<name>]` config block, OutRig should behave like `podman run` with
/// `--pull=never` and accept only already-local images.
pub async fn ensure_local_image(
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
    Err(OutrigError::Configuration(format!(
        "--image {:?} did not match any [images.<name>] and local podman image {:?} was not found",
        tag.0, tag.0
    )))
}

/// Pull an image by ref via `podman pull`. Stderr is streamed to
/// `tracing::info!` with the `[podman]` prefix.
pub async fn pull_image(tag: &ImageTag) -> Result<()> {
    let cmd = Cmd::new("podman").arg("pull").arg(&tag.0);
    let argv_for_error = cmd.args.clone();
    let status = process::run_streamed(cmd, "podman").await?;
    if !status.success() {
        return Err(OutrigError::Process {
            program: "podman",
            argv: argv_for_error,
            exit_code: status.code(),
            stderr_tail: String::new(),
        });
    }
    Ok(())
}

/// Logged sibling of [`pull_image`] for session startup.
async fn pull_image_logged(tag: &ImageTag, transcript: Option<&Transcript>) -> Result<()> {
    process::run_capture_logged(
        Cmd::new("podman").arg("pull").arg(&tag.0),
        "podman",
        transcript,
    )
    .await?;
    Ok(())
}

/// Run `buildah build` for `cfg`, tagging the result `tag`, with build-arg
/// resolution errors framed against the selected image-config.
pub async fn build_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
) -> Result<()> {
    let build_args = resolve_build_args(image, cfg)?;
    build_image_with_build_args(cfg, repo_root, tag, no_cache, &build_args).await
}

async fn build_image_with_build_args(
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, String>,
) -> Result<()> {
    let temp_tag = temporary_build_tag(tag);
    let result = async {
        let cmd = build_image_cmd(cfg, repo_root, &temp_tag, no_cache, build_args);
        let argv_for_error = cmd.args.clone();
        let status = process::run_streamed(cmd, "buildah").await?;
        if !status.success() {
            return Err(OutrigError::Process {
                program: "buildah",
                argv: argv_for_error,
                exit_code: status.code(),
                stderr_tail: String::new(),
            });
        }
        stamp_repo_image_labels(&temp_tag, tag, &cfg.mcp, None).await
    }
    .await;
    cleanup_temp_image(&temp_tag, None).await;
    result
}

async fn build_image_logged_with_build_args(
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
    build_args: &BTreeMap<String, String>,
) -> Result<()> {
    let temp_tag = temporary_build_tag(tag);
    let result = async {
        process::run_capture_logged(
            build_image_cmd(cfg, repo_root, &temp_tag, no_cache, build_args),
            "buildah",
            transcript,
        )
        .await?;
        stamp_repo_image_labels(&temp_tag, tag, &cfg.mcp, transcript).await
    }
    .await;
    cleanup_temp_image(&temp_tag, transcript).await;
    result
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
async fn ensure_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<ImageBuildOutcome> {
    match cfg.source() {
        ImageSourceRef::Image { image_name } => {
            let tag = ImageTag(image_name.to_string());
            if !no_cache && probe_pulled(&tag).await? {
                tracing::info!(target: "outrig::image", cache_hit = true, "ensured image {tag}");
                return Ok(ImageBuildOutcome {
                    tag,
                    cache_hit: true,
                });
            }
            pull_image(&tag).await?;
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
            build_image_with_build_args(cfg, repo_root, &tag, no_cache, &build_args).await?;
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
pub async fn ensure_tagged_image_for(
    image: &str,
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
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
            pull_image_logged(tag, transcript).await?;
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
                cfg,
                repo_root,
                tag,
                no_cache,
                transcript,
                &build_args,
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
/// image metadata. `dockerfile` and `context` are resolved relative to
/// `project_dir`. Stderr from buildah is streamed to `tracing::info!` with the
/// `[buildah]` prefix.
///
/// Unlike [`ensure_image`], there is no content-addressed cache probe: a
/// standalone build tags a stable caller-named ref (e.g. `rust-dev`), not a
/// `<name>:<key>` content-hash tag. `no_cache` therefore only forwards
/// `--no-cache` to buildah.
pub async fn build_standalone(
    project_dir: &Path,
    dockerfile: &Path,
    context: &Path,
    tag: &ImageTag,
    no_cache: bool,
    labels: &BTreeMap<String, String>,
) -> Result<()> {
    let dockerfile = project_dir.join(dockerfile);
    let context = project_dir.join(context);
    let cmd = buildah_build_cmd(
        &dockerfile,
        &context,
        tag,
        no_cache,
        &BTreeMap::new(),
        labels,
    );
    let argv_for_error = cmd.args.clone();
    let status = process::run_streamed(cmd, "buildah").await?;
    if !status.success() {
        return Err(OutrigError::Process {
            program: "buildah",
            argv: argv_for_error,
            exit_code: status.code(),
            stderr_tail: String::new(),
        });
    }
    Ok(())
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
    let cmd = Cmd::new("podman")
        .arg("image")
        .arg("inspect")
        .arg(&tag.0)
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
            "podman image inspect {tag}: invalid labels JSON: {source}"
        ))
    })
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
        Err(OutrigError::Io(source)) if source.kind() == ErrorKind::NotFound => {
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

fn temporary_build_tag(final_tag: &ImageTag) -> ImageTag {
    let (repo, key) = final_tag
        .0
        .rsplit_once(':')
        .unwrap_or((TAG_PREFIX, "image"));
    ImageTag(format!(
        "{repo}:outrig-tmp-{}-{}-{key}",
        std::process::id(),
        temp_nonce()
    ))
}

fn temporary_builder_name() -> String {
    format!("outrig-label-{}-{}", std::process::id(), temp_nonce())
}

fn temp_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

async fn stamp_repo_image_labels(
    source_tag: &ImageTag,
    final_tag: &ImageTag,
    config_mcp: &BTreeMap<String, McpServerSpec>,
    transcript: Option<&Transcript>,
) -> Result<()> {
    let inherited = read_image_labels(source_tag, transcript).await?;
    let labels = merged_mcp_config_to_labels(&source_tag.0, &inherited, config_mcp)?;
    commit_image_with_labels(source_tag, final_tag, &labels, transcript).await
}

async fn commit_image_with_labels(
    source_tag: &ImageTag,
    final_tag: &ImageTag,
    labels: &BTreeMap<String, String>,
    transcript: Option<&Transcript>,
) -> Result<()> {
    let builder = temporary_builder_name();
    let result = async {
        run_buildah_capture(
            Cmd::new("buildah")
                .arg("from")
                .arg("--pull=never")
                .arg("--name")
                .arg(&builder)
                .arg(&source_tag.0),
            transcript,
        )
        .await?;

        let mut config = Cmd::new("buildah").arg("config");
        for (key, value) in labels {
            config = config.arg("--label").arg(format!("{key}={value}"));
        }
        config = config.arg(&builder);
        run_buildah_capture(config, transcript).await?;

        run_buildah_capture(
            Cmd::new("buildah")
                .arg("commit")
                .arg("--rm")
                .arg("--quiet")
                .arg(&builder)
                .arg(&final_tag.0),
            transcript,
        )
        .await?;
        Ok(())
    }
    .await;

    if result.is_err() {
        cleanup_builder(&builder, transcript).await;
    }
    result
}

async fn run_buildah_capture(cmd: Cmd, transcript: Option<&Transcript>) -> Result<()> {
    if transcript.is_some() {
        process::run_capture_logged(cmd, "buildah", transcript).await?;
    } else {
        process::run_capture(cmd).await?;
    }
    Ok(())
}

async fn cleanup_builder(builder: &str, transcript: Option<&Transcript>) {
    let cmd = Cmd::new("buildah").arg("rm").arg(builder);
    if transcript.is_some() {
        let _ = process::try_capture_logged(cmd, "buildah", transcript).await;
    } else {
        let _ = process::try_capture(cmd).await;
    }
}

async fn cleanup_temp_image(tag: &ImageTag, transcript: Option<&Transcript>) {
    let cmd = Cmd::new("buildah").arg("rmi").arg(&tag.0);
    if transcript.is_some() {
        let _ = process::try_capture_logged(cmd, "buildah", transcript).await;
    } else {
        let _ = process::try_capture(cmd).await;
    }
}

fn build_image_cmd(
    cfg: &ImageConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, String>,
) -> Cmd {
    let dockerfile = repo_root.join(cfg.dockerfile.as_ref().expect("build path validated"));
    let context = repo_root.join(cfg.context.as_ref().expect("build path validated"));
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
/// [--build-arg ...] [--label ...] <context>` command. `dockerfile` and
/// `context` are absolute (already joined with their base dir). Used directly
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
    build_args: &BTreeMap<String, String>,
    labels: &BTreeMap<String, String>,
) -> Cmd {
    let mut cmd = Cmd::new("buildah")
        .arg("build")
        .arg("--tag")
        .arg(&tag.0)
        .arg("--file")
        .arg(dockerfile);
    if no_cache {
        cmd = cmd.arg("--no-cache");
    }
    for (k, v) in build_args {
        cmd = cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    for (k, v) in labels {
        cmd = cmd.arg("--label").arg(format!("{k}={v}"));
    }
    cmd.arg(context)
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

async fn hash_git_context(ctx: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    // `--full-name` makes `ls-files` emit paths relative to the repo root.
    // Without it, paths come out cwd-relative, but `hash-object --stdin-paths`
    // (below) only resolves repo-root-relative paths -- the two would
    // disagree whenever `ctx` is a subdirectory of the working tree.
    let listing = process::run_capture(Cmd::new("git").arg("-C").arg(ctx).args([
        "ls-files",
        "-z",
        "--full-name",
        ".",
    ]))
    .await?;

    // Sort defensively (`git ls-files` already sorts, but pin the order so
    // the hash stream is stable against any future change).
    let mut sorted: Vec<Vec<u8>> = listing
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    sorted.sort_unstable();

    let cmd = Cmd::new("git")
        .arg("-C")
        .arg(ctx)
        .args(["hash-object", "--stdin-paths"]);
    let argv_for_error = cmd.args.clone();
    let mut child = process::spawn_stdio(cmd).await?;

    // Write paths to stdin and drain stdout concurrently. Writing all stdin
    // before reading stdout would deadlock once the kernel pipe buffer fills
    // (~64 KiB on Linux, ~1600 typical paths' worth of output).
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let writer = tokio::spawn(async move {
        for p in &sorted {
            stdin.write_all(p).await?;
            stdin.write_all(b"\n").await?;
        }
        drop(stdin);
        Ok::<(), std::io::Error>(())
    });

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut buf = Vec::new();
    stdout.read_to_end(&mut buf).await?;

    let status = child.wait().await?;
    writer.await.expect("stdin writer panicked")?;

    if !status.success() {
        return Err(OutrigError::Process {
            program: "git",
            argv: argv_for_error,
            exit_code: status.code(),
            stderr_tail: String::new(),
        });
    }

    hasher.update(&buf);
    Ok(())
}

async fn hash_tar_context(ctx: &Path, hasher: &mut blake3::Hasher) -> Result<()> {
    // Reproducibility flags: --sort=name fixes file order, --mtime pins
    // archive mtimes to the epoch (otherwise filesystem mtimes leak in),
    // --owner=0/--group=0/--numeric-owner strip uid/gid (otherwise the
    // hash would differ across users / machines).
    let cmd = Cmd::new("tar")
        .args([
            "--sort=name",
            "--mtime=UTC 1970-01-01",
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "-cf",
            "-",
            "-C",
        ])
        .arg(ctx)
        .arg(".");
    let argv_for_error = cmd.args.clone();
    let mut child = process::spawn_stdio(cmd).await?;

    drop(child.stdin.take());

    let stderr = child.stderr.take().expect("stderr was piped");
    let stderr_task = tokio::spawn(async move {
        let mut s = stderr;
        let mut buf = Vec::new();
        let _ = s.read_to_end(&mut buf).await;
        if !buf.is_empty() {
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                tracing::warn!(target: "outrig::image", "[tar] {line}");
            }
        }
    });

    let mut stdout = child.stdout.take().expect("stdout was piped");
    let mut chunk = vec![0u8; TAR_READ_CHUNK];
    loop {
        let n = stdout.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk[..n]);
    }

    let status = child.wait().await?;
    let _ = stderr_task.await;
    if !status.success() {
        return Err(OutrigError::Process {
            program: "tar",
            argv: argv_for_error,
            exit_code: status.code(),
            stderr_tail: String::new(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::config::EnvValue;

    #[test]
    fn build_image_cmd_uses_resolved_build_args() {
        let cfg = ImageConfig {
            image_name: None,
            dockerfile: Some(PathBuf::from("Dockerfile")),
            context: Some(PathBuf::from(".")),
            build_args: BTreeMap::from([(
                "GH_TOKEN".to_string(),
                EnvValue::EnvRef("GITHUB_TOKEN".to_string()),
            )]),
            security: Default::default(),
            mcp: BTreeMap::new(),
        };
        let resolved = BTreeMap::from([("GH_TOKEN".to_string(), "secret-token".to_string())]);

        let cmd = build_image_cmd(
            &cfg,
            Path::new("/repo"),
            &ImageTag("outrig-cache:test".to_string()),
            false,
            &resolved,
        );

        assert!(cmd.args.contains(&OsString::from("--build-arg")));
        assert!(cmd.args.contains(&OsString::from("GH_TOKEN=secret-token")));
        assert!(
            !cmd.args
                .contains(&OsString::from("GH_TOKEN=${GITHUB_TOKEN}"))
        );
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

        let labels =
            read_remote_image_labels_with_program(program, "example.com/acme/rust-dev:latest")
                .await
                .expect("fake skopeo succeeds");

        assert_eq!(labels["org.outrig.tags"], r#"["remote"]"#);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_remote_image_labels_surfaces_registry_failures() {
        let program = fake_skopeo("echo registry auth required >&2\nexit 7\n");

        let err = read_remote_image_labels_with_program(program, "example.com/private:latest")
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
