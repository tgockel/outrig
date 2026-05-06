//! outrig: run LLM agents inside podman-managed containers.

pub mod cli;
pub mod config;
pub mod container;
pub mod error;
pub mod hf;
pub mod image;
pub mod init;
pub mod llm;
pub mod mcp;
pub mod process;
pub mod repl;
pub mod repo;
pub mod rig_tool;
pub mod session;
pub mod tool_name;
