//! Env-var values for MCP server `env` tables. Each value is either a literal
//! string, used as-is, or a `${VAR}` reference resolved from the host
//! environment when the MCP server is about to start. Unlike `ApiKeyRef`,
//! literals are accepted -- existing configs use them for in-container paths
//! like `CARGO_HOME = "/workspace/.cargo"`. The `${VAR}` form uses the same
//! syntax as `api-key` for consistency.

use std::env::VarError;

use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::env_ref::parse_env_ref;

#[derive(Debug, Error)]
pub enum EnvValueError {
    #[error("env var {var} is not set")]
    NotPresent { var: String },

    #[error("env var {var} value is not valid UTF-8")]
    NotUnicode { var: String },
}

/// A single entry of an MCP server's `env` table -- either a literal value
/// passed through verbatim, or a reference to a host env var resolved at
/// MCP-startup time.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl Serialize for EnvValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Self::Literal(s) => serializer.serialize_str(s),
            Self::EnvRef(var) => serializer.collect_str(&format_args!("${{{var}}}")),
        }
    }
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::from_raw(raw))
    }
}
