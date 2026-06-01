//! `outrig build`: pre-warm one (or every) image-config image.
//!
//! [`execute`] follows the order documented in `doc/usage/build.md`:
//! load + merge + validate config, decide the target list, then for each
//! target compute the cache tag, probe the image store, and either print
//! a one-line "cache hit" summary or stream a `buildah build` between the
//! verbose header and a final `image ready` line. `--all` prints a
//! per-image summary line instead of the verbose form so the output
//! stays scannable across many image-configs.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use clap::{ArgGroup, Parser};

use crate::error::{OutrigError, Result};
use crate::paths::repo_root_from_config_path;
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
    repo_cfg_path: &Path,
    global_cfg_path: &Path,
    args: &BuildArgs,
) -> Result<i32> {
    let repo_root = repo_root_from_config_path(repo_cfg_path);
    let cfg = Config::load(&repo_root, Some(global_cfg_path))?;

    let targets: Vec<&str> = if args.all {
        if cfg.images.is_empty() {
            return Err(OutrigError::Configuration(
                "--all requires at least one [images.<name>] block".to_string(),
            )
            .into());
        }
        cfg.images.keys().map(String::as_str).collect()
    } else {
        let name = args
            .image
            .as_deref()
            .or(cfg.default_image.as_deref())
            .ok_or_else(|| {
                OutrigError::Configuration(
                    "no --image, --all, or default-image configured".to_string(),
                )
            })?;
        vec![name]
    };

    if args.all {
        build_all(&cfg, &repo_root, &targets, args.no_cache).await
    } else {
        let name = targets[0];
        let cc = cfg.images.get(name).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "image-config {name:?} does not match any [images.<name>]"
            ))
        })?;
        build_single(name, cc, &repo_root, args.no_cache).await
    }
}

async fn build_single(
    name: &str,
    cc: &ImageConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<i32> {
    match cc.source() {
        ImageSourceRef::Image { image_name } => {
            let tag = image::ImageTag(image_name.to_string());
            let already_pulled = !no_cache && image::probe_pulled(&tag).await?;
            if already_pulled {
                eprintln!("[outrig] image ready (already pulled: {tag})");
                return Ok(0);
            }
            print_image_header(name, &tag);
            image::pull_image(&tag).await?;
            eprintln!("[outrig] image ready");
            Ok(0)
        }
        ImageSourceRef::Build { .. } => {
            let tag = image::compute_tag_for(name, cc, repo_root).await?;
            let cache_hit = !no_cache && image::probe_cached(&tag).await?;
            if cache_hit {
                eprintln!("[outrig] image ready (cache hit: {tag})");
                return Ok(0);
            }
            print_build_header(name, cc, &tag);
            image::build_image_for(name, cc, repo_root, &tag, no_cache).await?;
            eprintln!("[outrig] image ready");
            Ok(0)
        }
    }
}

async fn build_all(
    cfg: &Config,
    repo_root: &Path,
    targets: &[&str],
    no_cache: bool,
) -> Result<i32> {
    let pad = targets.iter().map(|n| n.len()).max().unwrap_or(0);
    for name in targets {
        let cc = cfg.images.get(*name).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "image-config {name:?} does not match any [images.<name>]"
            ))
        })?;
        match cc.source() {
            ImageSourceRef::Image { image_name } => {
                let tag = image::ImageTag(image_name.to_string());
                let already_pulled = !no_cache && image::probe_pulled(&tag).await?;
                let suffix = if already_pulled {
                    "(already pulled)".to_string()
                } else {
                    let started = Instant::now();
                    image::pull_image(&tag).await?;
                    format!("(pulled in {}s)", started.elapsed().as_secs())
                };
                eprintln!("[outrig] image-config: {name:<pad$} -> {tag} {suffix}");
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
                eprintln!("[outrig] image-config: {name:<pad$} -> {tag} {suffix}");
            }
        }
    }
    eprintln!("[outrig] all images ready");
    Ok(0)
}

fn print_build_header(name: &str, cc: &ImageConfig, tag: &ImageTag) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] image-config: {name}");
    let _ = writeln!(
        buf,
        "[outrig] dockerfile:       {}",
        cc.dockerfile.as_ref().expect("build path").display()
    );
    let _ = writeln!(
        buf,
        "[outrig] context:          {}",
        cc.context.as_ref().expect("build path").display()
    );
    let _ = writeln!(buf, "[outrig] cache key:        {tag}");
    eprint!("{buf}");
}

fn print_image_header(name: &str, tag: &ImageTag) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] image-config: {name}");
    let _ = writeln!(buf, "[outrig] image:            {tag}");
    eprint!("{buf}");
}
