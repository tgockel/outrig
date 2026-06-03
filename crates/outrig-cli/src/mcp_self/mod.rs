//! Self-description MCP server for `outrig mcp self`.
//!
//! This server runs on the host and exposes only read/advisory tools:
//! embedded docs, schema projection, suggested tools, and
//! non-mutating validators for proposed image-config artifacts.

#![deny(clippy::print_stdout)]

pub(crate) mod docs;
mod schema;
mod server;
mod suggestions;
pub(crate) mod validate;

pub use server::serve_stdio;
