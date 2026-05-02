//! Top-level error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("no .agents/outrig/config.toml found in current directory or any parent")]
    NoRepoConfig,

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Config(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, OutrigError>;
