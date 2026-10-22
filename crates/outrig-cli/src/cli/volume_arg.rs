//! Parse `--volume` CLI entries for `outrig run` and `outrig mcp`.
//!
//! Syntax: `HOST:CONTAINER[:ro|rw]`. Two segments use the default access
//! (read-only, matching config-file `[workspace.mounts]`); a third segment, if
//! present, must be exactly `ro` or `rw`. Host paths therefore cannot contain
//! `:` -- Windows drive-letter paths are unsupported, consistent with this
//! being a Linux/podman tool.
//!
//! Relative host paths are preserved verbatim here and resolved by
//! [`CliVolume::to_mount`] against the current directory -- the directory the
//! flag was typed in, which is the base a shell reads a relative path against.
//! A config mount resolves against the file that declared it instead, and a
//! `--volume` was declared in no file.

use std::path::{Path, PathBuf};

use outrig::config::{MountAccess, MountConfig};

/// A parsed `--volume HOST:CONTAINER[:ro|rw]` bind mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliVolume {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
}

impl CliVolume {
    /// This volume as a mount the session can append to
    /// `[workspace.mounts]`, with its host path already made absolute
    /// against `cwd`.
    ///
    /// The resolution happens here rather than at launch because a
    /// `MountConfig` built by hand carries no [`ConfigSource`], and launch
    /// resolves such an entry against the repo root -- the directory holding
    /// `.agents/outrig/`, which from a subdirectory is not where the user
    /// typed the flag. `--volume data:/data` from `<repo>/sub` then mounted
    /// `<repo>/data`, and with only `<repo>/sub/data` present it failed
    /// naming a path nobody typed (#367). The rule itself is config's own,
    /// applied to a base of the CLI's choosing: an absolute path is used
    /// as-is, a leading `~` is the home directory, and anything else joins
    /// `cwd`.
    ///
    /// [`ConfigSource`]: outrig::config::ConfigSource
    pub fn to_mount(&self, cwd: &Path) -> MountConfig {
        let mut mount = MountConfig::new(self.host.clone(), self.container.clone(), self.access);
        // `components()` drops the `.` a shell completion leaves on
        // `./data`, the way the workspace host path is spelled out, so a
        // diagnostic quotes a path a reader can compare to one they typed.
        // Lexical only: nothing here touches the filesystem, and validation
        // is what decides whether the directory is there.
        let host: PathBuf = mount.resolved_host_path(cwd).components().collect();
        // Written back so every later reader -- validation, the podman
        // launch, and any diagnostic that quotes `host-path` -- sees the one
        // directory this flag means.
        mount.set_host_path(host);
        mount
    }
}

/// clap `value_parser` for `--volume`.
pub fn parse_volume(raw: &str) -> Result<CliVolume, String> {
    let parts: Vec<&str> = raw.split(':').collect();
    let (host, container, access) = match parts.as_slice() {
        [host, container] => (*host, *container, MountAccess::ReadOnly),
        [host, container, mode] => {
            let access = match *mode {
                "ro" => MountAccess::ReadOnly,
                "rw" => MountAccess::ReadWrite,
                other => {
                    return Err(format!(
                        "invalid access {other:?} in --volume {raw:?}: expected `ro` or `rw`"
                    ));
                }
            };
            (*host, *container, access)
        }
        _ => {
            return Err(format!(
                "invalid --volume {raw:?}: expected HOST:CONTAINER[:ro|rw]"
            ));
        }
    };

    if host.is_empty() {
        return Err(format!("invalid --volume {raw:?}: empty host path"));
    }
    if container.is_empty() {
        return Err(format!("invalid --volume {raw:?}: empty container path"));
    }

    Ok(CliVolume {
        host: PathBuf::from(host),
        container: PathBuf::from(container),
        access,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_segments_default_to_read_only() {
        let v = parse_volume("/host/data:/data").unwrap();
        assert_eq!(v.host, PathBuf::from("/host/data"));
        assert_eq!(v.container, PathBuf::from("/data"));
        assert_eq!(v.access, MountAccess::ReadOnly);
    }

    #[test]
    fn explicit_rw_and_ro_are_honored() {
        assert_eq!(
            parse_volume("/h:/c:rw").unwrap().access,
            MountAccess::ReadWrite
        );
        assert_eq!(
            parse_volume("/h:/c:ro").unwrap().access,
            MountAccess::ReadOnly
        );
    }

    #[test]
    fn relative_host_path_is_preserved_verbatim() {
        // Resolution happens in `to_mount`, against a base the caller picks.
        let v = parse_volume("data:/data:rw").unwrap();
        assert_eq!(v.host, PathBuf::from("data"));
    }

    /// The base is the directory the flag was typed in, not the repo root a
    /// sourceless `MountConfig` would otherwise fall back to (#367).
    #[test]
    fn a_relative_host_path_resolves_against_the_given_directory() {
        let mount = parse_volume("data:/data:rw")
            .unwrap()
            .to_mount(Path::new("/repo/sub"));
        assert_eq!(mount.host_path(), Path::new("/repo/sub/data"));
        assert_eq!(mount.container_path(), Path::new("/data"));
        assert_eq!(mount.access(), MountAccess::ReadWrite);
        // Resolved once, here: a later resolution against any other base
        // finds the path already absolute and leaves it alone.
        assert_eq!(
            mount.resolved_host_path(Path::new("/elsewhere")),
            PathBuf::from("/repo/sub/data")
        );
    }

    #[test]
    fn a_dot_relative_host_path_loses_the_dot() {
        let mount = parse_volume("./data:/data")
            .unwrap()
            .to_mount(Path::new("/repo/sub"));
        assert_eq!(mount.host_path(), Path::new("/repo/sub/data"));
    }

    #[test]
    fn an_absolute_host_path_is_left_alone() {
        let mount = parse_volume("/host/data:/data")
            .unwrap()
            .to_mount(Path::new("/repo/sub"));
        assert_eq!(mount.host_path(), Path::new("/host/data"));
    }

    /// A `~` quoted past the shell is still the home directory, as it is in a
    /// config mount -- so it is not joined onto the current directory.
    #[test]
    fn a_leading_tilde_is_not_joined_onto_the_current_directory() {
        let cwd = Path::new("/repo/sub");
        let mount = parse_volume("~/data:/data").unwrap().to_mount(cwd);
        assert!(
            !mount.host_path().starts_with(cwd),
            "~ resolved under the current directory: {:?}",
            mount.host_path()
        );
    }

    #[test]
    fn rejects_unknown_access_segment() {
        let err = parse_volume("/h:/c:rwx").unwrap_err();
        assert!(err.contains("expected `ro` or `rw`"), "got: {err}");
    }

    #[test]
    fn rejects_too_many_segments() {
        let err = parse_volume("/h:/c:rw:extra").unwrap_err();
        assert!(err.contains("HOST:CONTAINER[:ro|rw]"), "got: {err}");
    }

    #[test]
    fn rejects_single_segment() {
        let err = parse_volume("/h").unwrap_err();
        assert!(err.contains("HOST:CONTAINER[:ro|rw]"), "got: {err}");
    }

    #[test]
    fn rejects_empty_host() {
        let err = parse_volume(":/c").unwrap_err();
        assert!(err.contains("empty host path"), "got: {err}");
    }

    #[test]
    fn rejects_empty_container() {
        let err = parse_volume("/h:").unwrap_err();
        assert!(err.contains("empty container path"), "got: {err}");
    }
}
