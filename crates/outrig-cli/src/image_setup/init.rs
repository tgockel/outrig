//! `outrig image init` -- noninteractive scaffolding of a standalone image project.
//!
//! Unlike `outrig image add` (which writes a repo-local image-config under
//! `.agents/outrig/images/<name>/` and mutates the repo `config.toml`), `init`
//! creates an *independent* project whose build output is a reusable container
//! image. It writes three files into a target directory -- `Dockerfile`,
//! `image.toml`, and (if absent) `README.md` -- and touches nothing else.
//!
//! The Dockerfile body is the same one `outrig image add` would render for a
//! Debian-slim base with the filesystem MCP server. The generated `image.toml`
//! stays beside the Dockerfile as the authoring source; `outrig image build`
//! validates it and stamps its config into OCI labels.

use std::path::Path;

use crate::error::{OutrigError, Result};
use crate::image_setup::render::{self, BaseImage, McpServer};
use crate::paths::write_atomic;
use outrig::config::check_build_image_name;

/// Scaffold a standalone image project in `dir` (relative to `cwd`, or `cwd`
/// itself when `dir` is `None`). The project name -- used as the `image.ref`
/// and in the README's consuming config -- is the target directory's basename.
///
/// Refuses to clobber `Dockerfile` or `image.toml` unless `force` is set;
/// the existence probe runs before any write so a re-run reports the conflict
/// without leaving a half-written project. An existing README is always preserved.
pub fn run(cwd: &Path, dir: Option<&Path>, force: bool) -> Result<()> {
    let target_dir = dir.map_or_else(|| cwd.to_path_buf(), |d| cwd.join(d));
    let name = derive_name(&target_dir)?;

    let dockerfile_path = target_dir.join("Dockerfile");
    let image_toml_path = target_dir.join("image.toml");
    let readme_path = target_dir.join("README.md");

    if !force {
        let existing: Vec<String> = [&dockerfile_path, &image_toml_path]
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
    if readme_path.exists() || readme_path.is_symlink() {
        eprintln!("[outrig] keeping existing {}", display_rel(&readme_path, cwd));
    } else {
        write_and_report(&readme_path, &render_readme(&name), cwd)?;
    }

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
    // The name becomes the image's ref, the repository `image build` tags it
    // with, so it's held to the same rule as a build image's name.
    if let Err(reason) = check_build_image_name(name) {
        return Err(OutrigError::Configuration(format!(
            "{name:?} is not a valid image name: {reason}; rename the directory or pass a \
             valid name."
        ))
        .into());
    }
    // Unlike a build image's, this ref carries no tag, and a bare one of 64
    // hex digits is an image ID to podman, which refuses it as a name.
    if name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(OutrigError::Configuration(format!(
            "{name:?} is not a valid image name: it becomes the image's ref, and podman \
             reads 64 hex digits as an image ID; rename the directory or pass a valid name."
        ))
        .into());
    }
    Ok(name.to_string())
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
    // A `.` in the name would nest a table in `[images.<name>]`, so the key is
    // quoted when it has to be. The `image-name` value is a string either way.
    let key = toml_edit::Key::new(name);
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
         Reference the built image from a repo's `.agents/outrig/config.toml` with `image-name`. The MCP servers are declared by the image labels, so no `[images.{key}.mcp]` block is needed:\n\
         \n\
         ```toml\n\
         [images.{key}]\n\
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

    use crate::mcp_self::validate::{validate_dockerfile, validate_image_toml};

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn init_with_dir_arg_creates_three_files() {
        for force in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            run(tmp.path(), Some(Path::new("rust-dev")), force).unwrap();

            let proj = tmp.path().join("rust-dev");
            assert!(proj.join("Dockerfile").is_file());
            assert!(proj.join("image.toml").is_file());
            assert_eq!(read(&proj.join("README.md")), render_readme("rust-dev"));
            assert!(read(&proj.join("image.toml")).contains("ref = \"rust-dev\""));
        }
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

    /// Podman refuses both as an image's ref: an uppercase repository, and a
    /// separator its reference grammar has no room for.
    #[test]
    fn invalid_directory_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["RustDev", "a..b"] {
            let err = run(tmp.path(), Some(Path::new(name)), false).unwrap_err();
            assert!(err.to_string().contains("not a valid image name"), "{err}");
            assert!(!tmp.path().join(name).exists(), "{name}");
        }
    }

    /// The ref is written without a tag, and buildah refuses a bare 64 hex
    /// digits as a name ("cannot specify 64-byte hexadecimal strings"), so
    /// neither passes, though the build-image rule allows both.
    #[test]
    fn an_image_id_shaped_name_errors() {
        let tmp = tempfile::tempdir().unwrap();
        for name in ["1".repeat(64), "a".repeat(64)] {
            let err = run(tmp.path(), Some(Path::new(&name)), false).unwrap_err();
            assert!(err.to_string().contains("as an image ID"), "{err}");
            assert!(!tmp.path().join(&name).exists(), "{name}");
        }
    }

    /// A `.` is allowed in the name, so the README's table header quotes it
    /// rather than nesting a table.
    #[test]
    fn a_dotted_name_scaffolds_a_readme_that_loads() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust.dev")), false).unwrap();
        let proj = tmp.path().join("rust.dev");

        let image_toml = read(&proj.join("image.toml"));
        assert!(image_toml.contains("ref = \"rust.dev\""), "{image_toml}");
        assert!(validate_image_toml(&image_toml).valid, "{image_toml}");

        let readme = read(&proj.join("README.md"));
        let (_, rest) = readme.split_once("```toml\n").expect("a toml block");
        let (snippet, _) = rest.split_once("```").expect("a closed toml block");
        assert_eq!(snippet, "[images.\"rust.dev\"]\nimage-name = \"rust.dev\"\n");
        let cfg = Config::load_from_str(snippet).expect("the snippet parses");
        cfg.validate(None).expect("the snippet validates");
        assert!(cfg.images.contains_key("rust.dev"), "{snippet}");
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
            validate_dockerfile(&dockerfile).warnings,
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
    fn existing_readme_does_not_block_init_and_is_never_overwritten() {
        for force in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let proj = tmp.path().join("rust-dev");
            std::fs::create_dir(&proj).unwrap();
            // Preserve arbitrary bytes, not just valid UTF-8 or a known template.
            let original = b"# My project\nHand-written notes\n\xff";
            std::fs::write(proj.join("README.md"), original).unwrap();

            run(tmp.path(), Some(Path::new("rust-dev")), force).unwrap();

            assert_eq!(std::fs::read(proj.join("README.md")).unwrap(), original);
            assert_eq!(read(&proj.join("Dockerfile")), render_dockerfile());
            assert_eq!(read(&proj.join("image.toml")), render_image_toml("rust-dev"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn existing_readme_symlink_is_preserved_even_when_dangling() {
        for force in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let proj = tmp.path().join("rust-dev");
            std::fs::create_dir(&proj).unwrap();
            let readme = proj.join("README.md");
            std::os::unix::fs::symlink("notes.md", &readme).unwrap();

            run(tmp.path(), Some(Path::new("rust-dev")), force).unwrap();

            assert_eq!(std::fs::read_link(&readme).unwrap(), Path::new("notes.md"));
            assert!(!proj.join("notes.md").exists());
        }
    }

    #[test]
    fn each_required_file_conflict_is_reported_before_any_write() {
        for filename in ["Dockerfile", "image.toml"] {
            let tmp = tempfile::tempdir().unwrap();
            let proj = tmp.path().join("rust-dev");
            std::fs::create_dir(&proj).unwrap();
            std::fs::write(proj.join(filename), "keep me").unwrap();

            let err = run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap_err();

            assert!(err.to_string().contains(filename), "{err}");
            assert!(err.to_string().contains("--force"), "{err}");
            assert_eq!(read(&proj.join(filename)), "keep me");
            assert_eq!(std::fs::read_dir(&proj).unwrap().count(), 1);
        }
    }

    #[test]
    fn force_overwrites_only_generated_files() {
        let tmp = tempfile::tempdir().unwrap();
        run(tmp.path(), Some(Path::new("rust-dev")), false).unwrap();

        let proj = tmp.path().join("rust-dev");
        let sentinel = proj.join("keep.txt");
        std::fs::write(&sentinel, "keep me").unwrap();
        // Force regenerates build inputs but preserves even a generated README.
        let readme = read(&proj.join("README.md"));
        std::fs::write(proj.join("Dockerfile"), "custom Dockerfile").unwrap();
        std::fs::write(proj.join("image.toml"), "garbage").unwrap();

        run(tmp.path(), Some(Path::new("rust-dev")), true).unwrap();

        assert_eq!(read(&sentinel), "keep me");
        assert_eq!(read(&proj.join("README.md")), readme);
        assert_eq!(read(&proj.join("Dockerfile")), render_dockerfile());
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
