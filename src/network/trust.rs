//! Install a per-session MITM CA into a session container's trust stores.
//!
//! Writes the CA PEM to three canonical locations covering Debian-family,
//! Red Hat-family, and a standalone PEM that language-specific bundle
//! variables can point at. Runs both system updaters with `|| true` so the
//! wrong-distro tool fails silently. Finally drops an `/etc/profile.d`
//! snippet exporting language env vars (`NODE_EXTRA_CA_CERTS`,
//! `REQUESTS_CA_BUNDLE`, `SSL_CERT_FILE`, `CURL_CA_BUNDLE`,
//! `GIT_SSL_CAINFO`) so shells inside the container pick the CA up.

use std::collections::BTreeMap;

use crate::container::{Container, podman_exec_root};
use crate::error::Result;
use crate::process;
#[cfg(test)]
use crate::process::Cmd;

/// Path inside `/etc/profile.d` that exports the language trust-store env
/// vars. Sourced by every interactive shell on POSIX-style images.
pub const CA_PROFILE_PATH: &str = "/etc/profile.d/outrig-ca.sh";

/// Path the standalone PEM lives at, regardless of distro. The standalone
/// PEM exists so tools that read a single CA file (notably Node via
/// `NODE_EXTRA_CA_CERTS`) can point at one path.
pub const STANDALONE_CA_PATH: &str = "/etc/ssl/certs/outrig-ca.pem";

/// Path that the merged system bundle lives at on both Debian-family and
/// Red Hat-family images. `update-ca-certificates` / `update-ca-trust`
/// rebuild it after our anchor is added.
pub const SYSTEM_BUNDLE_PATH: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Write the CA into the container's trust stores. Idempotent (rerunning
/// just rewrites the files with the same content).
pub async fn install_ca_in_container(container: &Container, ca_pem: &str) -> Result<()> {
    let script = build_install_script(ca_pem);
    let cmd = podman_exec_root(&container.name)
        .arg("sh")
        .arg("-c")
        .arg(script);
    let transcript = container.transcript();
    let _ = process::run_capture_logged(cmd, "network", transcript.as_ref()).await?;
    Ok(())
}

/// Env vars the interceptor must forward into every `podman exec` so MCP
/// servers pick up the CA without depending on `/etc/profile.d` being
/// sourced. The merged-bundle path covers requests/curl/git; the
/// standalone PEM covers Node.
pub fn exec_env_overrides() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "NODE_EXTRA_CA_CERTS".to_string(),
        STANDALONE_CA_PATH.to_string(),
    );
    for key in [
        "REQUESTS_CA_BUNDLE",
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "GIT_SSL_CAINFO",
    ] {
        env.insert(key.to_string(), SYSTEM_BUNDLE_PATH.to_string());
    }
    env
}

fn build_install_script(ca_pem: &str) -> String {
    // Single-quoted heredoc (`<<'EOF'`) keeps shell from interpreting `$`,
    // backticks, or backslashes in the PEM body. PEM data is base64 plus
    // header dashes plus whitespace -- it cannot contain the EOF marker we
    // pick below, and cannot contain `'` either. The trailing newline on
    // every literal is deliberate.
    let mut script = String::new();
    script.push_str("set -e\n");
    script.push_str("umask 022\n");
    script.push_str("mkdir -p /usr/local/share/ca-certificates ");
    script.push_str("/etc/pki/ca-trust/source/anchors /etc/ssl/certs ");
    script.push_str("/etc/profile.d\n");

    let pem_locations = [
        "/usr/local/share/ca-certificates/outrig-ca.crt",
        "/etc/pki/ca-trust/source/anchors/outrig-ca.crt",
        STANDALONE_CA_PATH,
    ];
    for path in pem_locations {
        script.push_str("cat > '");
        script.push_str(path);
        script.push_str("' <<'OUTRIG_CA_PEM_EOF'\n");
        script.push_str(ca_pem);
        if !ca_pem.ends_with('\n') {
            script.push('\n');
        }
        script.push_str("OUTRIG_CA_PEM_EOF\n");
    }

    script.push_str("update-ca-certificates >/dev/null 2>&1 || true\n");
    script.push_str("update-ca-trust extract >/dev/null 2>&1 || true\n");

    script.push_str("cat > '");
    script.push_str(CA_PROFILE_PATH);
    script.push_str("' <<'OUTRIG_PROFILE_EOF'\n");
    script.push_str(profile_script_body());
    script.push_str("OUTRIG_PROFILE_EOF\n");
    script.push_str("chmod 0644 ");
    script.push_str(CA_PROFILE_PATH);
    script.push('\n');

    script
}

fn profile_script_body() -> &'static str {
    "# outrig MITM trust anchors\n\
     export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/outrig-ca.pem\n\
     export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt\n\
     export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt\n\
     export CURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt\n\
     export GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt\n"
}

/// Strip the wrapper around `podman exec`, returning just the trailing
/// shell argv -- useful for unit tests that want to inspect what would be
/// executed without spawning podman. Kept here (rather than in the test
/// module) so call sites that need to assert wire format have one knob.
#[cfg(test)]
pub(crate) fn install_argv(container_name: &str, ca_pem: &str) -> Cmd {
    podman_exec_root(container_name)
        .arg("sh")
        .arg("-c")
        .arg(build_install_script(ca_pem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_writes_all_three_pem_paths() {
        let script =
            build_install_script("-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n");
        assert!(script.contains("/usr/local/share/ca-certificates/outrig-ca.crt"));
        assert!(script.contains("/etc/pki/ca-trust/source/anchors/outrig-ca.crt"));
        assert!(script.contains(STANDALONE_CA_PATH));
    }

    #[test]
    fn install_script_runs_both_updaters_with_or_true() {
        let script = build_install_script("PEM\n");
        assert!(script.contains("update-ca-certificates >/dev/null 2>&1 || true"));
        assert!(script.contains("update-ca-trust extract >/dev/null 2>&1 || true"));
    }

    #[test]
    fn install_script_drops_profile_with_node_var() {
        let script = build_install_script("PEM\n");
        assert!(script.contains(CA_PROFILE_PATH));
        assert!(script.contains("NODE_EXTRA_CA_CERTS=/etc/ssl/certs/outrig-ca.pem"));
        assert!(script.contains("REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt"));
        assert!(script.contains("GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt"));
    }

    #[test]
    fn install_script_appends_newline_to_pem() {
        let script = build_install_script("not-newline-terminated");
        // The heredoc body must end with a newline before the EOF marker,
        // otherwise the heredoc parser eats the marker. Look for at least
        // one occurrence of "not-newline-terminated\nOUTRIG_CA_PEM_EOF".
        assert!(script.contains("not-newline-terminated\nOUTRIG_CA_PEM_EOF\n"));
    }

    #[test]
    fn install_argv_includes_user_root_and_target_container() {
        let cmd = install_argv("outrig-test", "PEM\n");
        let rendered = cmd.render();
        assert!(rendered.contains("podman"));
        assert!(rendered.contains("--user=0:0"));
        assert!(rendered.contains("outrig-test"));
        assert!(rendered.contains("sh -c"));
    }

    #[test]
    fn exec_env_overrides_covers_node_and_bundle_vars() {
        let env = exec_env_overrides();
        assert_eq!(
            env.get("NODE_EXTRA_CA_CERTS").map(String::as_str),
            Some(STANDALONE_CA_PATH)
        );
        for key in [
            "REQUESTS_CA_BUNDLE",
            "SSL_CERT_FILE",
            "CURL_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            assert_eq!(
                env.get(key).map(String::as_str),
                Some(SYSTEM_BUNDLE_PATH),
                "missing or wrong: {key}"
            );
        }
    }
}
