//! How a keyed entry -- podman's `--env KEY=value`, buildah's
//! `--build-arg KEY=value` -- reaches the engine client, and how it is shown.
//!
//! A literal is passed and shown as written. A value resolved from a
//! `${VAR}` reference is shown as `KEY=${VAR}`, the config's own spelling, by
//! every diagnostic, and is passed **by name**: the argv carries the bare
//! `KEY`, and the value is set as `KEY` in the client's environment, which
//! both podman and buildah read a bare key from. A command line is readable
//! by every local user through `/proc/<pid>/cmdline` for as long as the client
//! runs -- which, for an exec-stdio MCP server's `podman exec`, is the whole
//! session -- while `/proc/<pid>/environ` is readable only by its owner.
//!
//! Passing by name puts the value in the client's own environment, so it is
//! only done for a key the client does not itself read. `HTTPS_PROXY` set
//! there for a server would also send podman's pulls through it,
//! `XDG_RUNTIME_DIR` would move its runtime state, `LD_PRELOAD` would load
//! into conmon and the OCI runtime, `_PODMAN_PAUSE` would turn podman into its
//! own pause process. Such a key keeps its value on the argv, as before
//! references were passed by name, and is still shown as the reference.
//!
//! The one exception is a proxy variable naming the same variable it
//! references, `HTTPS_PROXY = "${HTTPS_PROXY}"`: the value is then the one the
//! client already inherits, and both engines read proxies as they find them,
//! so passing it by name changes nothing. That holds for no other key on the
//! list. An engine that reads a variable may rewrite its own copy before the
//! bare key is looked up -- buildah makes a relative `TMPDIR` absolute -- and
//! what reached the container would no longer be what was resolved.
//!
//! The list is deliberately broad. A key on it that the engine never reads
//! costs exactly the exposure every reference had before; a key missing from
//! it silently changes how podman or buildah behaves.
//!
//! It is drawn from the source rather than from memory: every variable podman
//! 4.9.3 and 5.7.0 and buildah 1.33.7 and 1.42.1 read through `os.Getenv`,
//! `os.LookupEnv`, or C `getenv`, vendored libraries included, less those read
//! only on another platform or by a subsystem `create`, `exec`, and `build`
//! never reach -- `podman machine`, the API service, quadlet, the `atomic:`
//! transport, test tooling. Redo that sweep when a new engine release is
//! supported.

use crate::config::EnvValue;
use crate::process::Cmd;

/// Variables podman, buildah, or the processes they run read from their own
/// environment.
const ENGINE_READ: &[&str] = &[
    // The process itself and libc.
    "HOME",
    "PATH",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TZ",
    "TZDIR",
    "LANG",
    "LANGUAGE",
    "SUDO_USER",
    // Set by an engine in the containers it runs; buildah reads it to tell
    // whether it is in one.
    "container",
    // The Go runtime both are built on.
    "GODEBUG",
    "GOGC",
    "GOMAXPROCS",
    "GOMEMLIMIT",
    "GOTRACEBACK",
    // Go's TLS roots, which registry pulls verify against.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    // systemd, through go-systemd.
    "NOTIFY_SOCKET",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    // The containers stack. `REGISTRIES_CONFIG_PATH` replaces the registries
    // configuration libimage pulls with, `CI_DESIRED_DATABASE` the database
    // libpod opens, and `OPT` carries options into the re-executed layer
    // applier.
    "REGISTRY_AUTH_FILE",
    "REGISTRIES_CONFIG_PATH",
    "GNUPGHOME",
    "SSH_AUTH_SOCK",
    "SOURCE_DATE_EPOCH",
    "BUILD_REGISTRY_SOURCES",
    "CI_DESIRED_DATABASE",
    "SUPPRESS_BOLTDB_WARNING",
    "DISABLE_HC_SYSTEMD",
    "LOGLEVEL",
    "OPT",
    // Libraries the stack is built on: layer compression, the boltdb state
    // store, and the TOML parser every configuration file goes through.
    "MOBY_DISABLE_PIGZ",
    "BBOLT_VERIFY",
    "BURNTSUSHI_TOML_110",
];

/// Prefixes of variable families read the same way.
///
/// A bare `_` covers the stack's internal markers, which all begin with one:
/// `_CONTAINERS_*`, which rootless podman re-executes itself under,
/// `_PODMAN_PAUSE`, which podman's constructor reads before it parses a
/// single argument, and the `_OCI_*` pipes conmon is handed. `LISTEN_` is
/// systemd socket activation, whose descriptors podman passes on, and
/// `WATCHDOG_` its watchdog. `CLICOLOR` is read by the logger both engines
/// write through.
const ENGINE_READ_PREFIXES: &[&str] = &[
    "_",
    "LC_",
    "LD_",
    "XDG_",
    "LISTEN_",
    "WATCHDOG_",
    "DBUS_",
    "CLICOLOR",
    "CNI_",
    "OCICRYPT_",
    "CONTAINER_",
    "CONTAINERS_",
    "STORAGE_",
    "BUILDAH_",
    "PODMAN_",
    "DOCKER_",
];

/// Push `flag` and the entry `key` = `value` onto `cmd`.
///
/// `source` is the config value `value` was resolved from. Only a reference
/// changes anything; a literal, or no source at all, is the plain
/// `flag KEY=value` it always was.
pub(crate) fn push_keyed(
    cmd: Cmd,
    flag: &str,
    key: &str,
    value: &str,
    source: Option<&EnvValue>,
) -> Cmd {
    let cmd = cmd.arg(flag);
    let Some(EnvValue::EnvRef(var)) = source else {
        return cmd.arg(format!("{key}={value}"));
    };
    let shown = format!("{key}={}", shown_value(value, source));
    if by_name(key, var) {
        cmd.arg_shown_as(key, shown).env_hidden(key, value)
    } else {
        cmd.arg_shown_as(format!("{key}={value}"), shown)
    }
}

/// How a diagnostic shows `value`, resolved from `source`: a reference as the
/// config spelled it, anything else as written.
pub(crate) fn shown_value(value: &str, source: Option<&EnvValue>) -> String {
    match source {
        Some(source @ EnvValue::EnvRef(_)) => source.to_raw(),
        _ => value.to_owned(),
    }
}

/// Whether a reference's value can go through the client's environment.
///
/// The key has to be a plain variable name besides: podman reads a bare key
/// ending in `*` as a glob over its environment and trims whitespace from it,
/// and a key holding `=` would be split by the client into a different key
/// and a value.
fn by_name(key: &str, var: &str) -> bool {
    is_plain_env_name(key) && (!is_engine_read(key) || (key == var && is_proxy(key)))
}

/// `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_plain_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn is_engine_read(key: &str) -> bool {
    ENGINE_READ.contains(&key)
        || ENGINE_READ_PREFIXES.iter().any(|p| key.starts_with(p))
        || is_proxy(key)
}

/// Proxy variables are read in either case.
fn is_proxy(key: &str) -> bool {
    key.to_ascii_uppercase().ends_with("_PROXY")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn reference(var: &str) -> EnvValue {
        EnvValue::EnvRef(var.to_string())
    }

    /// The executed argv, the shown argv, and the child environment `push_keyed` produced.
    fn pushed(
        key: &str,
        value: &str,
        source: Option<&EnvValue>,
    ) -> (Vec<OsString>, Vec<OsString>, Vec<(String, String)>) {
        let cmd = push_keyed(Cmd::new("podman"), "--env", key, value, source);
        (cmd.exec_args().to_vec(), cmd.shown_args(), cmd.hidden_env())
    }

    fn os(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn a_literal_is_passed_and_shown_as_written() {
        for source in [None, Some(&EnvValue::Literal("v".to_string()))] {
            let (exec, shown, env) = pushed("KEY", "v", source);
            assert_eq!(exec, os(&["--env", "KEY=v"]));
            assert_eq!(shown, exec);
            assert!(env.is_empty());
        }
    }

    #[test]
    fn a_reference_is_passed_by_name_and_shown_as_the_reference() {
        let (exec, shown, env) = pushed("GH_TOKEN", "s3cret", Some(&reference("GITHUB_TOKEN")));
        assert_eq!(exec, os(&["--env", "GH_TOKEN"]));
        assert_eq!(shown, os(&["--env", "GH_TOKEN=${GITHUB_TOKEN}"]));
        assert_eq!(env, [("GH_TOKEN".to_string(), "s3cret".to_string())]);
    }

    #[test]
    fn an_empty_reference_is_passed_by_name_as_set_but_empty() {
        let (exec, _, env) = pushed("KEY", "", Some(&reference("VAR")));
        assert_eq!(exec, os(&["--env", "KEY"]));
        assert_eq!(env, [("KEY".to_string(), String::new())]);
    }

    #[test]
    fn a_proxy_naming_its_own_variable_is_passed_by_name() {
        for key in ["HTTPS_PROXY", "http_proxy", "NO_PROXY"] {
            let (exec, _, env) = pushed(key, "v", Some(&reference(key)));
            assert_eq!(exec, os(&["--env", key]), "{key}");
            assert_eq!(env, [(key.to_string(), "v".to_string())], "{key}");
        }
    }

    /// buildah makes its own relative `TMPDIR` absolute before it reads a bare
    /// `--build-arg TMPDIR`, so by name a `.` would reach the Dockerfile as
    /// the directory buildah ran in.
    #[test]
    fn any_other_engine_read_key_naming_its_own_variable_keeps_its_value() {
        for key in ["TMPDIR", "HOME", "XDG_RUNTIME_DIR", "_PODMAN_PAUSE"] {
            let (exec, shown, env) = pushed(key, ".", Some(&reference(key)));
            assert_eq!(exec, os(&["--env", &format!("{key}=.")]), "{key}");
            assert_eq!(shown, os(&["--env", &format!("{key}=${{{key}}}")]), "{key}");
            assert!(env.is_empty(), "{key}");
        }
    }

    #[test]
    fn an_engine_read_key_keeps_its_value_on_the_argv_but_shows_the_reference() {
        let prefixed = ENGINE_READ_PREFIXES.iter().map(|p| format!("{p}X"));
        let named = [
            "HTTP_PROXY",
            "https_proxy",
            "No_Proxy",
            "ALL_PROXY",
            "_PODMAN_PAUSE",
            "_CONTAINERS_USERNS_CONFIGURED",
            "_OCI_SYNCPIPE",
            "LISTEN_FDS",
            "REGISTRIES_CONFIG_PATH",
            "CI_DESIRED_DATABASE",
            "OPT",
            "container",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "CNI_PATH",
            "CLICOLOR_FORCE",
        ]
        .map(String::from);
        let keys = ENGINE_READ
            .iter()
            .map(|k| k.to_string())
            .chain(prefixed)
            .chain(named);
        for key in keys {
            let (exec, shown, env) = pushed(&key, "s3cret", Some(&reference("OTHER")));
            assert_eq!(exec, os(&["--env", &format!("{key}=s3cret")]), "{key}");
            assert_eq!(shown, os(&["--env", &format!("{key}=${{OTHER}}")]), "{key}");
            assert!(env.is_empty(), "{key}");
        }
    }

    /// By name, this build-arg would also be buildah's own registries
    /// override, and a path that exists only in the image would fail the
    /// pull of its base.
    #[test]
    fn an_aliased_registries_config_build_arg_keeps_its_value() {
        let cmd = push_keyed(
            Cmd::new("buildah"),
            "--build-arg",
            "REGISTRIES_CONFIG_PATH",
            "/etc/containers/registries.conf.d/in-image.conf",
            Some(&reference("TARGET_PATH")),
        );
        assert_eq!(
            cmd.exec_args(),
            os(&[
                "--build-arg",
                "REGISTRIES_CONFIG_PATH=/etc/containers/registries.conf.d/in-image.conf",
            ])
        );
        assert_eq!(
            cmd.shown_args(),
            os(&["--build-arg", "REGISTRIES_CONFIG_PATH=${TARGET_PATH}"])
        );
        assert!(cmd.hidden_env().is_empty());
    }

    #[test]
    fn a_key_that_is_not_a_plain_name_never_reaches_the_environment() {
        for key in ["", "a=b", "my-key", "1AB", "K*", " K", "K "] {
            let (exec, shown, env) = pushed(key, "s3cret", Some(&reference("VAR")));
            assert_eq!(exec, os(&["--env", &format!("{key}=s3cret")]), "{key:?}");
            assert_eq!(shown, os(&["--env", &format!("{key}=${{VAR}}")]), "{key:?}");
            assert!(env.is_empty(), "{key:?}");
        }
    }

    #[test]
    fn a_plain_name_is_a_letter_or_underscore_then_alphanumerics() {
        for key in ["K", "_", "_K1", "lower_case", "GH_TOKEN"] {
            assert!(is_plain_env_name(key), "{key}");
        }
    }
}
