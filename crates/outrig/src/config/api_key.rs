//! `${VAR}`-only API-key references. Validated at config-load time, resolved
//! from `std::env` at use time. Literal keys are refused unconditionally so
//! they cannot land in committed config files.

use std::env::VarError;

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::env_ref::parse_env_ref;
use crate::error::Result;

#[derive(Debug, Error)]
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

    pub fn resolve(&self) -> Result<String> {
        std::env::var(&self.0).map_err(|err| {
            let var = self.0.clone();
            match err {
                VarError::NotPresent => ApiKeyError::NotPresent { var },
                VarError::NotUnicode(_) => ApiKeyError::NotUnicode { var },
            }
            .into()
        })
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
