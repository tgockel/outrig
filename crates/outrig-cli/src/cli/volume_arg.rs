//! Parse `--volume` CLI entries for `outrig run` and `outrig mcp`.
//!
//! Syntax: `HOST:CONTAINER[:ro|rw]`. Two segments use the default access
//! (read-only, matching config-file `[workspace.mounts]`); a third segment, if
//! present, must be exactly `ro` or `rw`. Host paths therefore cannot contain
//! `:` -- Windows drive-letter paths are unsupported, consistent with this
//! being a Linux/podman tool.
//!
//! Relative host paths are preserved verbatim here and resolved against the
//! repo root / current directory at launch time, identically to config mounts.

use std::path::PathBuf;

use outrig::config::MountAccess;

/// A parsed `--volume HOST:CONTAINER[:ro|rw]` bind mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliVolume {
    pub host: PathBuf,
    pub container: PathBuf,
    pub access: MountAccess,
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
        // Resolution against repo_root/cwd happens at launch, not here.
        let v = parse_volume("data:/data:rw").unwrap();
        assert_eq!(v.host, PathBuf::from("data"));
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
