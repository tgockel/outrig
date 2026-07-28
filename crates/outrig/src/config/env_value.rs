//! Env-var values for config tables such as MCP server `env` entries and
//! Dockerfile `build-args`. Each value is either a literal string, used as-is,
//! or a `${VAR}` reference resolved from the host environment at the call site.
//! Unlike `ApiKeyRef`, literals are accepted -- existing configs use them for
//! in-container paths like `CARGO_HOME = "/workspace/.cargo"` and Dockerfile
//! args like `NODE_VERSION = "20"`. The `${VAR}` form uses the same syntax as
//! `api-key` for consistency.

use std::env::VarError;

use std::borrow::Cow;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::env_ref::parse_env_ref;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EnvValueError {
    #[error("env var {var} is not set")]
    NotPresent { var: String },

    #[error("env var {var} value is not valid UTF-8")]
    NotUnicode { var: String },
}

/// A single entry of a config value table -- either a literal value passed
/// through verbatim, or a reference to a host env var resolved at the call
/// site that needs the concrete string.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvValue {
    Literal(String),
    EnvRef(String),
}

impl EnvValue {
    /// A value is treated as a reference iff it whole-matches `${VAR}`;
    /// anything else (lower-case names, embedded `${X}`, unmatched braces) is
    /// a literal. There is no embedded-substitution syntax.
    pub fn from_raw(raw: String) -> Self {
        match parse_env_ref(&raw) {
            Some(var) => Self::EnvRef(var.to_string()),
            None => Self::Literal(raw),
        }
    }

    /// For `Literal`, returns the value. For `EnvRef`, reads `std::env::var`
    /// and maps `VarError` into a typed error naming the missing variable.
    pub fn resolve(&self) -> Result<String, EnvValueError> {
        match self {
            Self::Literal(s) => Ok(s.clone()),
            Self::EnvRef(var) => std::env::var(var).map_err(|err| match err {
                VarError::NotPresent => EnvValueError::NotPresent { var: var.clone() },
                VarError::NotUnicode(_) => EnvValueError::NotUnicode { var: var.clone() },
            }),
        }
    }
}

impl EnvValue {
    /// The config-file spelling this value round-trips through: the literal
    /// text, or `${VAR}` for a reference. Inverse of [`EnvValue::from_raw`].
    pub fn to_raw(&self) -> String {
        match self {
            Self::Literal(s) => s.clone(),
            Self::EnvRef(var) => format!("${{{var}}}"),
        }
    }
}

impl Serialize for EnvValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_raw())
    }
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_raw(raw))
    }
}

impl JsonSchema for EnvValue {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "EnvValue".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string" })
    }
}
