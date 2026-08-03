//! Self-description MCP server for `outrig mcp self`.
//!
//! This server runs on the host and exposes only read/advisory tools:
//! embedded docs, schema projection, suggested tools, and
//! non-mutating validators for proposed image-config artifacts.
//!
//! [`server`] is one of two front-ends over these modules. The other is
//! [`crate::self_tool`], which offers the same set to an in-session agent as
//! `outrig__*` built-ins -- which is why the four data modules are visible to
//! the crate and only the rmcp plumbing is private here.

#![deny(clippy::print_stdout)]

pub(crate) mod args;
pub(crate) mod docs;
pub(crate) mod schema;
pub(crate) mod server;
pub(crate) mod suggestions;
pub(crate) mod validate;

pub use server::serve_stdio;
