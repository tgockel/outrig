//! `outrig build`: pre-warm one (or every) image-config image.
//!
//! [`execute`] follows the order documented in `doc/usage/build.md`:
//! load + merge + validate the build-relevant config, decide the target list, then for each
//! target compute the cache tag, probe the image store, and either print
//! a one-line "cache hit" summary or stream a `buildah build` between the
//! verbose header and a final `image ready` line. `--all` prints a
//! per-image summary line instead of the verbose form so the output
//! stays scannable across many image-configs.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use clap::{ArgGroup, Parser};

use crate::builtin_image;
use crate::cli::signals::Signals;
use crate::error::{OutrigError, Result};
use crate::notice;
use crate::paths::RepoConfig;
use outrig::config::{Config, ImageConfig, ImageSourceRef};
use outrig::image::{self, ImageTag};

#[derive(Debug, Parser)]
#[command(group(ArgGroup::new("target").args(["image", "all"])))]
pub struct BuildArgs {
    /// Pick a `[images.<name>]` block. Defaults to `default-image` from config.
    #[arg(long, value_name = "NAME")]
    pub image: Option<String>,

    /// Build every image-config defined in the config file.
    #[arg(long)]
    pub all: bool,

    /// Force rebuild even on cache hit. Passes `--no-cache` to buildah.
    #[arg(long = "no-cache")]
    pub no_cache: bool,
}

/// Run one `outrig build` invocation end-to-end. Returns the process exit code.
pub async fn execute(
    repo: &RepoConfig,
    global_cfg_path: Option<&Path>,
    args: &BuildArgs,
) -> Result<i32> {
    // Install before the first engine operation. Dropping the whole invocation
    // also stops `--all` before it can move on to another image; the library's
    // process owner and temporary-tag guard handle the canceled build.
    let mut signals = Signals::install()?;
    notice::with_async_notices(async {
        let result = image::with_build_cleanup(signals.race_ending("the build", async {
            let result = execute_inner(repo, global_cfg_path, args).await;
            notice::flush_async().await;
            result
        }))
        .await;
        // The interruption notice must not wait on a hub/stderr lock held by
        // stalled output. Deliver it when possible, but never delay cleanup
        // or exit indefinitely for a reader that is still open but not reading.
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), notice::flush_async())
            .await;
        result
    })
    .await
}

async fn execute_inner(
    repo: &RepoConfig,
    global_cfg_path: Option<&Path>,
    args: &BuildArgs,
) -> Result<i32> {
    let repo_root = &repo.root;
    // Config warnings can write stderr too. Keep those synchronous reads and
    // diagnostics off the signal-driving executor along with hash work.
    let owned_repo = repo.clone();
    let global_cfg_path = global_cfg_path.map(Path::to_path_buf);
    let mut cfg =
        tokio::task::spawn_blocking(move || owned_repo.load_for_build(global_cfg_path.as_deref()))
            .await
            .expect("build config task panicked")?;

    // `--all` means "every image-config *you* declared". outrig's built-in
    // default is not that, and pulling and building it in every repo that runs
    // `outrig build --all` would be a surprise -- so this returns before the
    // injection below, and `--all` never sees it. The built-in stays reachable
    // by name, which is the deliberate way to pre-warm it.
    if args.all {
        if cfg.images.is_empty() {
            return Err(OutrigError::Configuration(
                "--all requires at least one [images.<name>] block".to_string(),
            )
            .into());
        }
        return build_all(&cfg, repo_root, args.no_cache).await;
    }

    let name = args
        .image
        .as_deref()
        .or(cfg.default_image.as_deref())
        .ok_or_else(|| {
            OutrigError::Configuration(
                "no --image, --all, or default-image configured (pass \
                 --image outrig-default to pre-warm outrig's built-in default)"
                    .to_string(),
            )
        })?
        .to_string();
    if builtin_image::is_reserved(&name) {
        for note in &builtin_image::inject(&mut cfg).notes {
            notice::line_async(format!("[outrig] {note}")).await;
        }
    }

    let cc = cfg.images.get(&name).ok_or_else(|| {
        OutrigError::Configuration(format!(
            "image-config {name:?} does not match any [images.<name>]"
        ))
    })?;
    build_single(&name, cc, repo_root, args.no_cache).await
}

async fn build_single(
    name: &str,
    cc: &ImageConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<i32> {
    match cc.source() {
        ImageSourceRef::Image { image_name, .. } => {
            let tag = image::ImageTag::new(image_name);
            let already_pulled = !no_cache && image::probe_pulled(&tag).await?;
            if already_pulled {
                notice::line_async(format!("[outrig] image ready (already pulled: {tag})")).await;
                return Ok(0);
            }
            print_image_header(name, &tag).await;
            image::pull_image(&tag).await?;
            notice::line_async(format!("[outrig] image ready: {tag}")).await;
            Ok(0)
        }
        ImageSourceRef::Build {
            dockerfile,
            context,
            ..
        } => {
            let tag = image::compute_tag_for(name, cc, repo_root).await?;
            let cache_hit = !no_cache && image::probe_cached(&tag).await?;
            if cache_hit {
                notice::line_async(format!("[outrig] image ready (cache hit: {tag})")).await;
                return Ok(0);
            }
            print_build_header(name, dockerfile, context, &tag).await;
            image::build_image_for(name, cc, repo_root, &tag, no_cache).await?;
            notice::line_async(format!("[outrig] image ready: {tag}")).await;
            Ok(0)
        }
        // `ImageSourceRef` is `#[non_exhaustive]`; a source kind this build
        // cannot produce an image from is an error, not a silent no-op.
        _ => Err(unsupported_image_source(name).into()),
    }
}

/// The error for an image-config whose source this build does not understand.
fn unsupported_image_source(name: &str) -> OutrigError {
    OutrigError::Configuration(format!(
        "image-config {name:?} uses an image source this build does not support"
    ))
}

async fn build_all(cfg: &Config, repo_root: &Path, no_cache: bool) -> Result<i32> {
    let pad = cfg.images.keys().map(String::len).max().unwrap_or(0);
    for (name, cc) in &cfg.images {
        match cc.source() {
            ImageSourceRef::Image { image_name, .. } => {
                let tag = image::ImageTag::new(image_name);
                let already_pulled = !no_cache && image::probe_pulled(&tag).await?;
                let suffix = if already_pulled {
                    "(already pulled)".to_string()
                } else {
                    let started = Instant::now();
                    image::pull_image(&tag).await?;
                    format!("(pulled in {}s)", started.elapsed().as_secs())
                };
                notice::line_async(format!(
                    "[outrig] image-config: {name:<pad$} -> {tag} {suffix}"
                ))
                .await;
            }
            ImageSourceRef::Build { .. } => {
                let tag = image::compute_tag_for(name, cc, repo_root).await?;
                let cache_hit = !no_cache && image::probe_cached(&tag).await?;
                let suffix = if cache_hit {
                    "(cache hit)".to_string()
                } else {
                    let started = Instant::now();
                    image::build_image_for(name, cc, repo_root, &tag, no_cache).await?;
                    format!("(built in {}s)", started.elapsed().as_secs())
                };
                notice::line_async(format!(
                    "[outrig] image-config: {name:<pad$} -> {tag} {suffix}"
                ))
                .await;
            }
            _ => return Err(unsupported_image_source(name).into()),
        }
    }
    notice::line_async("[outrig] all images ready".to_string()).await;
    Ok(0)
}

/// Takes the two paths rather than the whole [`ImageConfig`] because the only
/// caller has already destructured [`ImageSourceRef::Build`], which is where
/// the build shape is established. Reaching back through the config would have
/// to re-assert it.
async fn print_build_header(name: &str, dockerfile: &Path, context: &Path, tag: &ImageTag) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] image-config: {name}");
    let _ = writeln!(buf, "[outrig] dockerfile:       {}", dockerfile.display());
    let _ = writeln!(buf, "[outrig] context:          {}", context.display());
    let _ = writeln!(buf, "[outrig] image tag:        {tag}");
    notice::write_async(buf).await;
}

async fn print_image_header(name: &str, tag: &ImageTag) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] image-config: {name}");
    let _ = writeln!(buf, "[outrig] image:            {tag}");
    notice::write_async(buf).await;
}
