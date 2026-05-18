//! Image build via buildah with content-addressed cache.
//!
//! The cache-key helper produces a deterministic 16-hex-char key over
//! `(Dockerfile bytes, resolved build-args, context content)`. [`ensure_image`]
//! probes `outrig-cache:<key>` first; on miss it shells out to
//! `buildah build`. Buildah's own layer cache still helps speed up the build
//! itself when we miss; the project-level tag cache exists so a *hit* skips
//! buildah entirely.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{ContainerConfig, ContainerSourceRef};
use crate::error::{OutrigError, Result};
use crate::process::{self, Cmd, Transcript};

const TAG_PREFIX: &str = "outrig-cache";
const KEY_HEX_LEN: usize = 16;
const TAR_READ_CHUNK: usize = 64 * 1024;
const UNNAMED_CONTAINER: &str = "<container>";

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
    /// relative-to-repo-root before calling.
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
}

/// Resolve `build-args` for a container-config. Literal values pass through;
/// `${VAR}` references are read from the host environment and framed with the
/// container name plus the build-arg key on failure.
pub(crate) fn resolve_build_args(
    container: &str,
    cfg: &ContainerConfig,
) -> Result<BTreeMap<String, String>> {
    let mut resolved = BTreeMap::new();
    for (key, value) in &cfg.build_args {
        let value = value
            .resolve()
            .map_err(|source| OutrigError::BuildArgResolveFailed {
                container: container.to_string(),
                key: key.clone(),
                source,
            })?;
        resolved.insert(key.clone(), value);
    }
    Ok(resolved)
}

/// Compute the deterministic `outrig-cache:<key>` tag for `cfg` without
/// touching buildah. For image-name configs, the tag is the literal
/// `image-name` value. This resolves `build-args` first because the cache key
/// tracks the concrete values passed to buildah. Useful when a caller wants
/// to print the tag *before* deciding whether to build (e.g. the
/// `outrig build` CLI's verbose header in `doc/usage/build.md`).
pub async fn compute_tag(cfg: &ContainerConfig, repo_root: &Path) -> Result<ImageTag> {
    compute_tag_for(UNNAMED_CONTAINER, cfg, repo_root).await
}

/// Named-container variant of [`compute_tag`]. Use this when config-derived
/// build args may need `${VAR}` resolution so errors can identify the source
/// container-config.
pub async fn compute_tag_for(
    container: &str,
    cfg: &ContainerConfig,
    repo_root: &Path,
) -> Result<ImageTag> {
    match cfg.source() {
        ContainerSourceRef::Image { image_name } => Ok(ImageTag(image_name.to_string())),
        ContainerSourceRef::Build { .. } => {
            let build_args = resolve_build_args(container, cfg)?;
            compute_tag_with_build_args(cfg, repo_root, &build_args).await
        }
    }
}

async fn compute_tag_with_build_args(
    cfg: &ContainerConfig,
    repo_root: &Path,
    build_args: &BTreeMap<String, String>,
) -> Result<ImageTag> {
    let dockerfile = repo_root.join(cfg.dockerfile.as_ref().expect("build path validated"));
    let context = repo_root.join(cfg.context.as_ref().expect("build path validated"));
    let key = CacheKey::compute(&dockerfile, build_args, &context).await?;
    Ok(ImageTag(format!("{TAG_PREFIX}:{key}")))
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
/// resolution errors framed against the selected container-config.
pub async fn build_image_for(
    container: &str,
    cfg: &ContainerConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
) -> Result<()> {
    let build_args = resolve_build_args(container, cfg)?;
    build_image_with_build_args(cfg, repo_root, tag, no_cache, &build_args).await
}

async fn build_image_with_build_args(
    cfg: &ContainerConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, String>,
) -> Result<()> {
    let cmd = build_image_cmd(cfg, repo_root, tag, no_cache, build_args);
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

async fn build_image_logged_with_build_args(
    cfg: &ContainerConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
    build_args: &BTreeMap<String, String>,
) -> Result<()> {
    process::run_capture_logged(
        build_image_cmd(cfg, repo_root, tag, no_cache, build_args),
        "buildah",
        transcript,
    )
    .await?;
    Ok(())
}

/// Probe `outrig-cache:<key>`; on miss (or when `no_cache` is set), run
/// `buildah build`. Stderr from buildah is streamed to `tracing::info!`
/// with the `[buildah]` prefix.
pub async fn ensure_image(
    cfg: &ContainerConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<ImageBuildOutcome> {
    ensure_image_for(UNNAMED_CONTAINER, cfg, repo_root, no_cache).await
}

/// Implementation of [`ensure_image`] with build-arg resolution errors framed
/// against the selected container-config.
async fn ensure_image_for(
    container: &str,
    cfg: &ContainerConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<ImageBuildOutcome> {
    match cfg.source() {
        ContainerSourceRef::Image { image_name } => {
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
        ContainerSourceRef::Build { .. } => {
            let build_args = resolve_build_args(container, cfg)?;
            let tag = compute_tag_with_build_args(cfg, repo_root, &build_args).await?;
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
/// framed against the selected container-config. This lets session startup
/// write a complete `session.json` and open `logs/container.log` before the
/// buildah probe/build begins, without hashing the Dockerfile/context twice.
pub async fn ensure_tagged_image_for(
    container: &str,
    cfg: &ContainerConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    transcript: Option<&Transcript>,
) -> Result<ImageBuildOutcome> {
    match cfg.source() {
        ContainerSourceRef::Image { .. } => {
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
        ContainerSourceRef::Build { .. } => {
            let build_args = resolve_build_args(container, cfg)?;
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

fn build_image_cmd(
    cfg: &ContainerConfig,
    repo_root: &Path,
    tag: &ImageTag,
    no_cache: bool,
    build_args: &BTreeMap<String, String>,
) -> Cmd {
    let dockerfile = repo_root.join(cfg.dockerfile.as_ref().expect("build path validated"));
    let context = repo_root.join(cfg.context.as_ref().expect("build path validated"));

    let mut cmd = Cmd::new("buildah")
        .arg("build")
        .arg("--tag")
        .arg(&tag.0)
        .arg("--file")
        .arg(&dockerfile);
    if no_cache {
        cmd = cmd.arg("--no-cache");
    }
    for (k, v) in build_args {
        cmd = cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    cmd.arg(&context)
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
    use std::path::PathBuf;

    use super::*;
    use crate::config::EnvValue;

    #[test]
    fn build_image_cmd_uses_resolved_build_args() {
        let cfg = ContainerConfig {
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
}

#[cfg(test)]
#[path = "image_cache_tests.rs"]
mod image_cache_tests;

#[cfg(test)]
#[path = "build_args_env_value_tests.rs"]
mod build_args_env_value_tests;
