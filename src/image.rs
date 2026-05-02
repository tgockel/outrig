//! Image build via buildah with content-addressed cache.
//!
//! [`CacheKey::compute`] produces a deterministic 16-hex-char key over
//! `(Dockerfile bytes, build-args, context content)`. [`ensure_image`]
//! probes `outrig-cache:<key>` first; on miss it shells out to
//! `buildah build`. Buildah's own layer cache still helps speed up the build
//! itself when we miss; the project-level tag cache exists so a *hit* skips
//! buildah entirely.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::ContainerConfig;
use crate::error::{OutrigError, Result};
use crate::process::{self, Cmd};

const TAG_PREFIX: &str = "outrig-cache";
const KEY_HEX_LEN: usize = 16;
const TAR_READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageTag(pub String);

impl fmt::Display for ImageTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct CacheKey;

impl CacheKey {
    /// Hash `(Dockerfile bytes, sorted build-args, context content)` into a
    /// 16-hex-char blake3 prefix. Caller passes absolute paths; `ensure_image`
    /// resolves relative-to-repo-root before calling.
    pub async fn compute(
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

/// Probe `outrig-cache:<key>`; on miss, run `buildah build`. Stderr from
/// buildah is streamed to `tracing::info!` with the `[buildah]` prefix.
pub async fn ensure_image(cfg: &ContainerConfig, repo_root: &Path) -> Result<ImageTag> {
    let dockerfile = repo_root.join(&cfg.dockerfile);
    let context = repo_root.join(&cfg.context);
    let key = CacheKey::compute(&dockerfile, &cfg.build_args, &context).await?;
    let tag = format!("{TAG_PREFIX}:{key}");

    let probe =
        process::try_capture(Cmd::new("buildah").args(["images", "--quiet"]).arg(&tag)).await?;
    if probe.status.success() && !probe.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(ImageTag(tag));
    }

    let mut cmd = Cmd::new("buildah")
        .arg("build")
        .arg("--tag")
        .arg(&tag)
        .arg("--file")
        .arg(&dockerfile);
    for (k, v) in &cfg.build_args {
        cmd = cmd.arg("--build-arg").arg(format!("{k}={v}"));
    }
    cmd = cmd.arg(&context);

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
    Ok(ImageTag(tag))
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
    let listing = process::run_capture(
        Cmd::new("git")
            .arg("-C")
            .arg(ctx)
            .args(["ls-files", "-z", "."]),
    )
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
