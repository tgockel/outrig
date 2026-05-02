//! Top-level error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OutrigError {
    #[error("not implemented: {0}")]
    NotImplemented(&'static str),
}

pub type Result<T> = std::result::Result<T, OutrigError>;
