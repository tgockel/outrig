//! Integration tests for the config schema: round-trip parsing, unknown-key
//! rejection, and MCP shape parity.

use std::collections::BTreeMap;
use std::path::PathBuf;

use outrig::config::{
    CapabilityProfile, Config, EnvValue, LlmProvider, McpServerSpec, MountAccess,
};
use outrig::error::OutrigError;

const FIXTURE: &str = include_str!("fixtures/config-full.toml");

mod config_schema {
    use super::*;

    #[test]
    fn fixture_round_trips_through_serde() {
        let parsed = Config::load_from_str(FIXTURE).expect("fixture parses");
        let reserialized = toml::to_string(&parsed).expect("config serializes");
        let again = Config::load_from_str(&reserialized).expect("reserialized parses");
        assert_eq!(parsed, again);
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let bad = r#"
default-agent = "coding"
oops          = "this key is not in the schema"
"#;
        let err = Config::load_from_str(bad).unwrap_err();
        let OutrigError::Config(toml_err) = err else {
            panic!("expected OutrigError::Config, got: {err:?}");
        };
        let msg = toml_err.to_string();
        assert!(
            msg.contains("oops"),
            "error message should point at the offending key, got: {msg}",
        );
    }

    #[test]
    fn mcp_short_and_full_normalize_equal() {
        let short_toml = r#"
[containers.c]
dockerfile = "D"
context    = "ctx"

[containers.c.mcp]
srv = ["bin", "arg1"]
"#;
        let full_toml = r#"
[containers.c]
dockerfile = "D"
context    = "ctx"

[containers.c.mcp]
srv = { command = ["bin", "arg1"] }
"#;

        let short = Config::load_from_str(short_toml).expect("short form parses");
        let full = Config::load_from_str(full_toml).expect("full form parses");

        let short_spec = short.containers["c"].mcp["srv"].clone();
        let full_spec = full.containers["c"].mcp["srv"].clone();

        assert!(
            matches!(short_spec, McpServerSpec::Short(_)),
            "array form should parse to Short, got: {short_spec:?}",
        );
        assert!(
            matches!(full_spec, McpServerSpec::Full { .. }),
            "table form should parse to Full, got: {full_spec:?}",
        );

        assert_eq!(short_spec.normalize(), full_spec.normalize());
        assert_eq!(
            short.containers["c"].mcp["srv"].normalize(),
            (vec!["bin".to_string(), "arg1".to_string()], BTreeMap::new()),
        );
    }

    #[test]
    fn fixture_spot_checks_match_documented_keys() {
        let cfg = Config::load_from_str(FIXTURE).expect("fixture parses");

        assert_eq!(cfg.default_container.as_deref(), Some("coding"));
        assert_eq!(cfg.default_agent.as_deref(), Some("coding"));
        assert_eq!(cfg.default_model.as_deref(), Some("fast"));
        assert_eq!(
            cfg.session_root.as_deref(),
            Some(std::path::Path::new("/var/lib/outrig/sessions")),
        );
        assert_eq!(
            cfg.model_cache_root.as_deref(),
            Some(std::path::Path::new("/var/cache/outrig/models")),
        );
        assert_eq!(cfg.tool_call_cap, Some(100));
        assert_eq!(cfg.tool_result_cap, Some(524288));

        let LlmProvider::OpenAi {
            base_url,
            api_key,
            request_timeout_secs,
        } = &cfg.providers["openai"]
        else {
            panic!("expected OpenAi variant for [providers.openai]");
        };
        assert_eq!(base_url, "https://api.openai.com/v1");
        assert_eq!(api_key.var_name(), "OPENAI_API_KEY");
        assert_eq!(*request_timeout_secs, Some(90));
        let LlmProvider::OpenAi {
            request_timeout_secs: anthropic_timeout,
            ..
        } = &cfg.providers["anthropic"]
        else {
            panic!("expected OpenAi variant for [providers.anthropic]");
        };
        assert_eq!(*anthropic_timeout, None);

        assert_eq!(cfg.models["fast"].provider, "openai");
        assert_eq!(
            cfg.models["fast"].identifier.as_deref(),
            Some("gpt-4o-mini")
        );

        // mistralrs-side models carry weight fields and no identifier.
        let phi3 = &cfg.models["phi3-fast"];
        assert_eq!(phi3.provider, "local");
        assert_eq!(phi3.identifier, None);
        assert_eq!(
            phi3.model_id.as_deref(),
            Some("microsoft/Phi-3-mini-4k-instruct-gguf")
        );
        assert_eq!(
            phi3.model_file.as_deref(),
            Some(&["Phi-3-mini-4k-instruct-q4.gguf".to_string()][..])
        );
        let llama = &cfg.models["llama-local"];
        assert_eq!(llama.provider, "local");
        assert_eq!(llama.identifier, None);
        assert_eq!(
            llama.model_path.as_deref(),
            Some(std::path::Path::new(
                ".agents/outrig/models/llama-3-8b-instruct.q4.gguf"
            )),
        );
        assert_eq!(llama.context_length, Some(4096));
        assert!(matches!(cfg.providers["local"], LlmProvider::Mistralrs));

        let coding = &cfg.agents["coding"];
        assert_eq!(coding.model, None);
        assert_eq!(coding.container.as_deref(), Some("coding"));
        assert_eq!(coding.temperature, Some(0.2));
        assert_eq!(coding.max_tokens, Some(4096));
        assert_eq!(coding.tool_call_cap, Some(300));
        assert_eq!(coding.tool_result_cap, Some(1048576));
        assert_eq!(cfg.agents["review"].model.as_deref(), Some("smart"));

        assert_eq!(cfg.workspace.host_path, PathBuf::from("."));
        assert_eq!(cfg.workspace.container_path, PathBuf::from("/workspace"));
        assert_eq!(cfg.workspace.mounts.len(), 2);
        assert_eq!(
            cfg.workspace.mounts[0].host_path,
            PathBuf::from(".agents/outrig/resources/docs"),
        );
        assert_eq!(
            cfg.workspace.mounts[0].container_path,
            PathBuf::from("/resources/docs"),
        );
        assert_eq!(cfg.workspace.mounts[0].access, MountAccess::ReadOnly);
        assert_eq!(
            cfg.workspace.mounts[1].host_path,
            PathBuf::from(".agents/outrig/resources/cache"),
        );
        assert_eq!(
            cfg.workspace.mounts[1].container_path,
            PathBuf::from("/resources/cache"),
        );
        assert_eq!(cfg.workspace.mounts[1].access, MountAccess::ReadWrite);

        let coding_ctr = &cfg.containers["coding"];
        assert_eq!(
            coding_ctr.dockerfile,
            Some(PathBuf::from(".agents/outrig/containers/coding/Dockerfile")),
        );
        assert_eq!(
            coding_ctr.context,
            Some(PathBuf::from(".agents/outrig/containers/coding")),
        );
        // Inner map keys (build-args ARG names, mcp env-var names) keep user
        // casing -- they're not subject to the outer `rename_all = kebab-case`.
        assert_eq!(
            coding_ctr.build_args["NODE_VERSION"],
            EnvValue::Literal("20".to_string()),
        );
        assert_eq!(
            coding_ctr.security.capability_profile,
            CapabilityProfile::NoNetRaw,
        );
        assert_eq!(coding_ctr.security.cap_drop, ["MKNOD", "SETFCAP"]);
        assert_eq!(coding_ctr.security.cap_add, ["NET_BIND_SERVICE"]);

        assert!(matches!(coding_ctr.mcp["shell"], McpServerSpec::Short(_)));
        let (fs_cmd, fs_env) = coding_ctr.mcp["fs"].normalize();
        assert_eq!(fs_cmd, vec!["mcp-server-filesystem", "/workspace"]);
        assert!(fs_env.is_empty());
        let (build_cmd, build_env) = coding_ctr.mcp["build"].normalize();
        assert_eq!(build_cmd, vec!["cargo-mcp"]);
        assert_eq!(
            build_env["CARGO_HOME"],
            EnvValue::Literal("/workspace/.cargo".to_string()),
        );
    }

    #[test]
    fn workspace_table_absent_yields_documented_defaults() {
        let cfg = Config::load_from_str("").expect("empty config parses");
        assert_eq!(cfg.workspace.host_path, PathBuf::from("."));
        assert_eq!(cfg.workspace.container_path, PathBuf::from("/workspace"));
        assert!(cfg.workspace.mounts.is_empty());
    }

    #[test]
    fn container_security_absent_yields_documented_defaults() {
        let cfg = Config::load_from_str(
            r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"
"#,
        )
        .expect("config parses");
        let security = &cfg.containers["coding"].security;
        assert_eq!(security.capability_profile, CapabilityProfile::Default);
        assert!(security.cap_drop.is_empty());
        assert!(security.cap_add.is_empty());
    }

    #[test]
    fn capability_profile_values_are_validated_by_schema() {
        let bad = r#"
[containers.coding]
dockerfile = "D"
context    = "ctx"

[containers.coding.security]
capability-profile = "wide-open"
"#;
        let err = Config::load_from_str(bad).unwrap_err();
        let OutrigError::Config(toml_err) = err else {
            panic!("expected OutrigError::Config, got: {err:?}");
        };
        let msg = toml_err.to_string();
        assert!(
            msg.contains("wide-open") || msg.contains("unknown variant"),
            "error should explain invalid capability profile, got: {msg}",
        );
    }

    #[test]
    fn workspace_mount_access_values_are_validated_by_schema() {
        let bad = r#"
[[workspace.mounts]]
host-path      = "docs"
container-path = "/resources/docs"
access         = "write-mostly"
"#;
        let err = Config::load_from_str(bad).unwrap_err();
        let OutrigError::Config(toml_err) = err else {
            panic!("expected OutrigError::Config, got: {err:?}");
        };
        let msg = toml_err.to_string();
        assert!(
            msg.contains("write-mostly") || msg.contains("unknown variant"),
            "error should explain invalid mount access, got: {msg}",
        );
    }
}
