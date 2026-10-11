//! `${VAR}`-only API-key references. Validated at config-load time, resolved
//! at use time -- from `std::env`, or from whatever [`Secrets`] a session was
//! given. Literal keys are refused unconditionally so they cannot land in
//! committed config files.

use std::env::VarError;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::env_ref::parse_env_ref;
use crate::error::Result;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ApiKeyError {
    #[error(
        "api-key {value:?} is invalid; expected \"${{VAR}}\" where VAR matches \
         ^[A-Z_][A-Z0-9_]*$"
    )]
    InvalidSyntax { value: String },

    #[error("api-key env var {var} is not set")]
    NotPresent { var: String },

    #[error("api-key env var {var} value is not valid UTF-8")]
    NotUnicode { var: String },

    /// A session's [`Secrets`] had no value for it.
    #[error("api-key ${{{var}}} has no value in this session's secrets")]
    Unresolved { var: String },
}

/// Where a session's `${VAR}` secrets come from: called while its model is
/// resolved, before anything starts, with each variable its model calls need
/// -- today each candidate provider's `api-key` -- and possibly more than once
/// for one name.
///
/// The process environment is one table for every session and thread in the
/// process, so an embedder running sessions that need different keys gives
/// each its own resolver and changes no environment at all. A key is read
/// once, at start, and held for the session's life: a rotated key takes a new
/// session.
pub trait Secrets: Send + Sync {
    fn resolve(&self, var: &str) -> std::result::Result<String, ApiKeyError>;
}

/// The process environment, with the errors reading it always gave. What
/// `outrig run-new` resolves through.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSecrets;

impl Secrets for EnvSecrets {
    fn resolve(&self, var: &str) -> std::result::Result<String, ApiKeyError> {
        std::env::var(var).map_err(|err| {
            let var = var.to_string();
            match err {
                VarError::NotPresent => ApiKeyError::NotPresent { var },
                VarError::NotUnicode(_) => ApiKeyError::NotUnicode { var },
            }
        })
    }
}

/// A function from a variable's name to its value; `None` is
/// [`ApiKeyError::Unresolved`].
impl<F> Secrets for F
where
    F: Fn(&str) -> Option<String> + Send + Sync,
{
    fn resolve(&self, var: &str) -> std::result::Result<String, ApiKeyError> {
        self(var).ok_or_else(|| ApiKeyError::Unresolved {
            var: var.to_string(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRef(String);

impl ApiKeyRef {
    pub fn parse(raw: &str) -> Result<Self> {
        let var = parse_env_ref(raw).ok_or_else(|| ApiKeyError::InvalidSyntax {
            value: raw.to_string(),
        })?;
        Ok(Self(var.to_string()))
    }

    /// The key, read from the process environment.
    pub fn resolve(&self) -> Result<String> {
        self.resolve_with(&EnvSecrets)
    }

    /// The key, as `secrets` gives it.
    pub fn resolve_with(&self, secrets: &(impl Secrets + ?Sized)) -> Result<String> {
        Ok(secrets.resolve(&self.0)?)
    }

    pub fn var_name(&self) -> &str {
        &self.0
    }
}

impl Serialize for ApiKeyRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("${{{}}}", self.0))
    }
}

impl<'de> Deserialize<'de> for ApiKeyRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}
