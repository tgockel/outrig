//! CLI subcommand entry points. Each subcommand owns its arg struct and its
//! `execute` function; `bin/outrig.rs` stays a thin dispatch table.

pub mod run;
