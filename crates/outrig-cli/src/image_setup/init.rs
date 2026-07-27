//! `outrig image init` -- noninteractive scaffolding of a standalone image project.
//!
//! Unlike `outrig image add` (which writes a repo-local image-config under
//! `.agents/outrig/images/<name>/` and mutates the repo `config.toml`), `init`
//! creates an *independent* project whose build output is a reusable container
//! image. It writes three files into a target directory -- `Dockerfile`,
//! `image.toml`, and `README.md` -- and touches nothing else.
//!
//! The Dockerfile body is the same one `outrig image add` would render for a
//! Debian-slim base with the filesystem MCP server. The generated `image.toml`
//! stays beside the Dockerfile as the authoring source; `outrig image build`
//! validates it and stamps its config into OCI labels.

use std::path::Path;

use crate::error::{OutrigError, Result};
use crate::image_setup::render::{self, BaseImage, McpServer};
use crate::paths::write_atomic;

/// Scaffold a standalone image project in `dir` (relative to `cwd`, or `cwd`
/// itself when `dir` is `None`). The project name -- used as the `image.ref`
/// and in the README's consuming config -- is the target directory's basename.
///
/// Refuses to clobber any of the three generated files unless `force` is set;
/// the existence probe runs before any write so a re-run reports the conflict
/// without leaving a half-written project.
pub fn run(cwd: &Path, dir: Option<&Path>, force: bool) -> Result<()> {
    let target_dir = dir.map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
    let name = derive_name(&target_dir)?;

    let dockerfile_path = target_dir.join("Dockerfile");
    let image_toml_path = target_dir.join("image.toml");
    let readme_path = target_dir.join("README.md");

    if !force {
        let existing: Vec<String> = [&dockerfile_path, &image_toml_path, &readme_path]
            .into_iter()
            .filter(|p| p.exists())
            .map(|p| display_rel(p, cwd).to_string())
            .collect();
        if !existing.is_empty() {
            return Err(OutrigError::Configuration(format!(
                "{} already present; pass --force to overwrite.",
                existing.join(", ")
            ))
            .into());
        }
    }

    write_and_report(&dockerfile_path, &render_dockerfile(), cwd)?;
    write_and_report(&image_toml_path, &render_image_toml(&name), cwd)?;
    write_and_report(&readme_path, &render_readme(&name), cwd)?;

    eprintln!("[outrig] next: build this image with `outrig image build`");
    Ok(())
}

/// Derive the project name from the target directory's basename. `Path`'s
/// component iterator normalizes away `.` (so `init .` and the no-arg form
/// yield the current directory's name) but preserves a trailing `..`, for which
/// -- and for `/` -- `file_name()` is `None` and we refuse rather than guess.
fn derive_name(target_dir: &Path) -> Result<String> {
    let name = target_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            OutrigError::Configuration(format!(
                "could not derive a project name from {}; pass a directory name, \
                 e.g. `outrig image init rust-dev`",
                target_dir.display()
            ))
        })?;
    if !is_valid_project_name(name) {
        return Err(OutrigError::Configuration(format!(
            "{name:?} is not a valid image name (must match ^[a-zA-Z][a-zA-Z0-9_-]*$); \
             rename the directory or pass a valid name."
        ))
        .into());
    }
    Ok(name.to_string())
}

/// `^[a-zA-Z][a-zA-Z0-9_-]*$` -- the same shape `image add` documents for
/// image names. Keeps the generated `image.toml` ref a clean token and the
/// README's `[images.<name>]` a valid TOML bare key.
fn is_valid_project_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Render the standalone Dockerfile: the curated Debian-slim + filesystem-MCP
/// body from `render`.
fn render_dockerfile() -> String {
    render::render(BaseImage::DebianBookwormSlim, &[], &[McpServer::Fs])
}

fn render_image_toml(name: &str) -> String {
    format!(
        "# OutRig standalone image config. `outrig image build` validates this file\n\
         # and stamps its MCP config into OCI labels. Required: [image].ref and a non-empty [mcp].\n\
         \n\
         [image]\n\
         ref = \"{name}\"\n\
         # description = \"...\"\n\
         # version = \"0.1.0\"\n\
         # tags = [\"...\"]\n\
         \n\
         [mcp]\n\
         fs = {{ command = [\"mcp-server-filesystem\", \"/workspace\"] }}\n"
    )
}

fn render_readme(name: &str) -> String {
    // One source line per output line keeps the prose free of string-continuation
    // whitespace surprises; lines are long but this is generated Markdown, not a doc/ page.
    format!(
        "# {name}\n\
         \n\
         An OutRig standalone toolset image. Building this project produces a container image (`{name}`) that bundles the agent's tools and MCP servers, ready to reference from any repo's OutRig config.\n\
         \n\
         ## Files\n\
         \n\
         - `Dockerfile`: the image definition. Follows OutRig conventions -- no `USER`, ends with `CMD [\"sleep\", \"infinity\"]`, and installs the host-UID bootstrap packages.\n\
         - `image.toml`: image metadata plus the `[mcp]` table. With no `[build]` section it builds the sibling `Dockerfile` with context `.`. `outrig image build` stamps this config into OCI labels.\n\
         - `README.md`: this file.\n\
         \n\
         ## Build\n\
         \n\
         ```sh\n\
         outrig image build\n\
         ```\n\
         \n\
         Run from this directory. It builds the `Dockerfile`, tags the image as `{name}`, stamps the `image.toml` config into OCI labels, and verifies the labeled MCP servers.\n\
         \n\
         ## Use it from a repo\n\
         \n\
         Reference the built image from a repo's `.agents/outrig/config.toml` with `image-name`. The MCP servers are declared by the image labels, so no `[images.{name}.mcp]` block is needed:\n\
         \n\
         ```toml\n\
         [images.{name}]\n\
         image-name = \"{name}\"\n\
         ```\n"
    )
}

fn write_and_report(path: &Path, contents: &str, cwd: &Path) -> Result<()> {
    write_atomic(path, contents)?;
    eprintln!("[outrig] wrote {}", display_rel(path, cwd));
    Ok(())
}

fn display_rel<'a>(path: &'a Path, root: &Path) -> std::path::Display<'a> {
    path.strip_prefix(root).unwrap_or(path).display()
}

#[cfg(test)]
mod tests {
    use super::*;

    use outrig::config::Config;
    use outrig::container::embedded::parse_standalone_image_toml;

    use crate::mcp_self::validate::{UserBootstrap, validate_dockerfile, validate_image_toml};

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn init_with_dir_arg_creates_three_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();

        let proj = tmp.path().join("rust-dev");
        assert!(proj.join("Dockerfile").is_file());
        assert!(proj.join("image.toml").is_file());
        assert!(proj.join("README.md").is_file());
        assert!(read(&proj.join("image.toml")).contains("ref = \"rust-dev\""));
    }

    #[test]
    fn name_defaults_to_cwd_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("web-tools");
        // dir = None -> name from cwd basename; write_atomic creates the dir.
        run(&cwd, None, false).unwrap();
        assert!(read(&cwd.join("image.toml")).contains("ref = \"web-tools\""));
    }

    #[test]
    fn init_dot_uses_cwd_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("api-image");
        std::fs::create_dir_all(&cwd).unwrap();
        run(&cwd, Some(Path::new(".")), false).unwrap();
        assert!(read(&cwd.join("image.toml")).contains("ref = \"api-image\""));
    }

    #[test]
    fn parent_dir_arg_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let err = run(&cwd, Some(Path::new("..")), false).unwrap_err();
        assert!(
            err.to_string().contains("could not derive a project name"),
            "{err}"
        );
    }

    #[test]
    fn invalid_directory_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(tmp.path(), Some(Path::new("123-foo")), false).unwrap_err();
        assert!(err.to_string().contains("not a valid image name"), "{err}");
    }

    #[test]
    fn generated_image_toml_passes_validator() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();
        let image_toml = read(&tmp.path().join("rust-dev/image.toml"));

        let result = validate_image_toml(&image_toml);
        assert!(result.valid, "{result:?}");
        assert!(parse_standalone_image_toml(&image_toml).is_ok());
    }

    #[test]
    fn generated_dockerfile_passes_validator() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();
        let dockerfile = read(&tmp.path().join("rust-dev/Dockerfile"));

        assert_eq!(
            validate_dockerfile(&dockerfile, UserBootstrap::HostSide).warnings,
            Vec::new()
        );
        assert!(dockerfile.starts_with("FROM docker.io/library/debian:bookworm-slim"));
        assert!(dockerfile.contains("npm install -g @modelcontextprotocol/server-filesystem"));
        assert!(!dockerfile.contains("COPY image.toml"), "{dockerfile}");

        assert!(
            dockerfile
                .trim_end()
                .ends_with("CMD [\"sleep\", \"infinity\"]")
        );
        assert!(!dockerfile.contains("USER "), "{dockerfile}");
    }

    #[test]
    fn render_footer_token_is_unique() {
        let body = render::render(BaseImage::DebianBookwormSlim, &[], &[McpServer::Fs]);
        assert_eq!(body.matches("WORKDIR /workspace").count(), 1, "{body}");
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();

        let err = run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--force"), "{msg}");
        assert!(msg.contains("Dockerfile"), "{msg}");
    }

    #[test]
    fn force_overwrites_only_generated_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();

        let proj = tmp.path().join("rust-dev");
        let sentinel = proj.join("keep.txt");
        std::fs::write(&sentinel, "keep me").unwrap();
        // Clobber a generated file to prove --force regenerates it.
        std::fs::write(proj.join("image.toml"), "garbage").unwrap();

        run(tmp.path(), Some(Path::new("rust-dev")), true).unwrap();

        assert_eq!(read(&sentinel), "keep me");
        assert!(read(&proj.join("image.toml")).contains("ref = \"rust-dev\""));
    }

    #[test]
    fn readme_documents_image_name_consuming_config() {
        let readme = render_readme("rust-dev");
        // The consuming-config block uses image-name with no mcp sub-block:
        // image-name immediately follows the [images.<name>] header.
        assert!(
            readme.contains("[images.rust-dev]\nimage-name = \"rust-dev\""),
            "{readme}"
        );
        assert!(
            readme.contains("stamps this config into OCI labels"),
            "{readme}"
        );
    }

    #[test]
    fn consuming_repo_config_is_valid() {
        // The shape the README documents: image-name, no [images.<name>.mcp].
        let snippet = "[images.rust-dev]\nimage-name = \"rust-dev\"\n";
        let cfg = Config::load_from_str(snippet).expect("config parses");
        cfg.validate(None).expect("config validates");
    }
}
