//! Self-description MCP server for `outrig mcp self`.
//!
//! This server runs on the host and exposes only read/advisory tools:
//! embedded docs, schema projection, suggested tools, and
//! non-mutating validators for proposed container-config artifacts.

#![deny(clippy::print_stdout)]

mod docs;
mod schema;
mod server;
mod suggestions;
mod validate;

pub use server::serve_stdio;
