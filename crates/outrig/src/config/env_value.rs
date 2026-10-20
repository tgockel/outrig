//! Env-var values for config tables such as MCP server `env` entries and
//! Dockerfile `build-args`. Each value is either a literal string, used as-is,
//! or a `${VAR}` reference resolved from the host environment at the call site.
//! Unlike `ApiKeyRef`, literals are accepted -- existing configs use them for
//! in-container paths like `CARGO_HOME = "/workspace/.cargo"` and Dockerfile
//! args like `NODE_VERSION = "20"`. The `${VAR}` form uses the same syntax as
//! `api-key` for consistency.

use std::env::VarError;

use std::borrow::Cow;
use std::fmt;

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

    /// The host variable this value references when `allow` does not list it,
    /// which is what `[repo] env-allow` refuses. A literal references nothing.
    ///
    /// The one definition of that refusal, for a reference in a repo config
    /// file and in a server an image's `org.outrig.mcp` label declares alike.
    /// Matched without a `_` arm: a new way to name a host variable has to say
    /// here whether it needs listing.
    pub(crate) fn unlisted_ref(&self, allow: &[String]) -> Option<&str> {
        match self {
            Self::EnvRef(var) if !allow.contains(var) => Some(var),
            Self::EnvRef(_) | Self::Literal(_) => None,
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

/// An [`EnvValue`] resolved against the host environment, kept beside the
/// value it was resolved from.
///
/// The source is what lets a resolved `${VAR}` reach podman or buildah without
/// being shown: a command built from one passes the key by name, with the
/// value in the client's environment rather than on its command line, and
/// every diagnostic spells the entry `KEY=${VAR}` as the config did. A plain
/// `String` carries no such record, so it is passed and shown as written.
///
/// The fields are private so the pair cannot drift: a value edited after
/// resolution would still claim the reference that never produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedEnvValue {
    value: String,
    source: EnvValue,
}

impl ResolvedEnvValue {
    /// Resolve `source` as [`EnvValue::resolve`] does, keeping it.
    pub fn resolve(source: EnvValue) -> Result<Self, EnvValueError> {
        let value = source.resolve()?;
        Ok(Self { value, source })
    }

    /// The resolved value: the literal, or the referenced variable's value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The config value this was resolved from.
    pub fn source(&self) -> &EnvValue {
        &self.source
    }

    pub(crate) fn into_value(self) -> String {
        self.value
    }

    /// A pair as if `source` had resolved to `value`, without reading the
    /// process environment.
    #[cfg(test)]
    pub(crate) fn assume(source: EnvValue, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            source,
        }
    }
}

/// Only the source: a resolved reference's value is what this type exists to
/// keep out of output.
impl fmt::Debug for ResolvedEnvValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedEnvValue")
            .field("source", &self.source)
            .finish_non_exhaustive()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_reference_is_refused_unless_the_list_names_it() {
        let value = EnvValue::from_raw("${TOKEN}".to_string());

        assert_eq!(value.unlisted_ref(&[]), Some("TOKEN"));
        assert_eq!(value.unlisted_ref(&names(&["OTHER"])), Some("TOKEN"));
        assert_eq!(value.unlisted_ref(&names(&["OTHER", "TOKEN"])), None);
    }

    /// A value that is not exactly `${VAR}` is a literal, passed verbatim, so
    /// it reads nothing from the host and has nothing to list.
    #[test]
    fn a_literal_names_no_variable() {
        for raw in ["/workspace/.cargo", "prefix-${TOKEN}-suffix", "${lower}"] {
            let value = EnvValue::from_raw(raw.to_string());

            assert_eq!(value.unlisted_ref(&[]), None, "{raw:?}");
        }
    }
}
