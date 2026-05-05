//! `outrig build`: pre-warm one (or every) container-config image.
//!
//! [`execute`] follows the order documented in `doc/usage/build.md`:
//! load + merge + validate config, decide the target list, then for each
//! target compute the cache tag, probe the image store, and either print
//! a one-line "cache hit" summary or stream a `buildah build` between the
//! verbose header and a final `image ready` line. `--all` prints a
//! per-container summary line instead of the verbose form so the output
//! stays scannable across many container-configs.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use clap::{ArgGroup, Parser};

use crate::config::{Config, ContainerConfig};
use crate::error::{OutrigError, Result};
use crate::image::{self, ImageTag};
use crate::repo;

#[derive(Debug, Parser)]
#[command(group(ArgGroup::new("target").args(["container", "all"])))]
pub struct BuildArgs {
    /// Pick a `[containers.<name>]` block. Defaults to `default-container` from config.
    #[arg(long, value_name = "NAME")]
    pub container: Option<String>,

    /// Build every container-config defined in the config file.
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
    let repo_root = repo::repo_root_from_config_path(repo_cfg_path);
    let cfg = Config::load(&repo_root, Some(global_cfg_path))?;

    let targets: Vec<&str> = if args.all {
        if cfg.containers.is_empty() {
            return Err(OutrigError::Configuration(
                "--all requires at least one [containers.<name>] block".to_string(),
            ));
        }
        cfg.containers.keys().map(String::as_str).collect()
    } else {
        let name = args
            .container
            .as_deref()
            .or(cfg.default_container.as_deref())
            .ok_or_else(|| {
                OutrigError::Configuration(
                    "no --container, --all, or default-container configured".to_string(),
                )
            })?;
        vec![name]
    };

    if args.all {
        build_all(&cfg, &repo_root, &targets, args.no_cache).await
    } else {
        let name = targets[0];
        let cc = cfg.containers.get(name).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container-config {name:?} does not match any [containers.<name>]"
            ))
        })?;
        build_single(name, cc, &repo_root, args.no_cache).await
    }
}

async fn build_single(
    name: &str,
    cc: &ContainerConfig,
    repo_root: &Path,
    no_cache: bool,
) -> Result<i32> {
    let tag = image::compute_tag(cc, repo_root).await?;
    let cache_hit = !no_cache && image::probe_cached(&tag).await?;

    if cache_hit {
        eprintln!("[outrig] image ready (cache hit: {tag})");
        return Ok(0);
    }

    print_build_header(name, cc, &tag);
    image::build_image(cc, repo_root, &tag, no_cache).await?;
    eprintln!("[outrig] image ready");
    Ok(0)
}

async fn build_all(
    cfg: &Config,
    repo_root: &Path,
    targets: &[&str],
    no_cache: bool,
) -> Result<i32> {
    let pad = targets.iter().map(|n| n.len()).max().unwrap_or(0);
    for name in targets {
        let cc = cfg.containers.get(*name).ok_or_else(|| {
            OutrigError::Configuration(format!(
                "container-config {name:?} does not match any [containers.<name>]"
            ))
        })?;
        let tag = image::compute_tag(cc, repo_root).await?;
        let cache_hit = !no_cache && image::probe_cached(&tag).await?;
        let suffix = if cache_hit {
            "(cache hit)".to_string()
        } else {
            let started = Instant::now();
            image::build_image(cc, repo_root, &tag, no_cache).await?;
            format!("(built in {}s)", started.elapsed().as_secs())
        };
        eprintln!("[outrig] container-config: {name:<pad$} -> {tag} {suffix}");
    }
    eprintln!("[outrig] all images ready");
    Ok(0)
}

fn print_build_header(name: &str, cc: &ContainerConfig, tag: &ImageTag) {
    let mut buf = String::new();
    let _ = writeln!(buf, "[outrig] container-config: {name}");
    let _ = writeln!(
        buf,
        "[outrig] dockerfile:       {}",
        cc.dockerfile.display()
    );
    let _ = writeln!(buf, "[outrig] context:          {}", cc.context.display());
    let _ = writeln!(buf, "[outrig] cache key:        {tag}");
    eprint!("{buf}");
}
