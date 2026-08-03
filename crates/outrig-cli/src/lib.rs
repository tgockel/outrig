//! Internals of the `outrig` CLI binary. End users should depend on the
//! [`outrig`] crate (the library) instead -- the only path this crate
//! promises is [`run`].
//!
//! The module tree is crate-private unless the `internal-test-api` feature
//! is on. The integration tests in `tests/` are separate crates, so they
//! turn it on through a dev-dependency on this crate; a published
//! `outrig-cli = "0.2"` dependency does not, and nothing behind it is
//! covered by SemVer.

/// Declares a module at one of two visibilities: `pub` under
/// `internal-test-api`, `pub(crate)` otherwise. A macro because visibility is
/// not an attribute, so `cfg_attr` cannot express this and the written-out
/// form is two `#[cfg]` arms per module.
macro_rules! internal_modules {
    ($($name:ident),+ $(,)?) => {
        $(
            #[cfg(feature = "internal-test-api")]
            pub mod $name;
            #[cfg(not(feature = "internal-test-api"))]
            pub(crate) mod $name;
        )+
    };
}

// Exactly the modules some integration test names.
internal_modules! {
    cli, config_init, error, hf, image_setup, init, llm, repl, rig_tool, session,
    session_tool,
}

// Reached only from inside the crate, so these stay private either way.
pub(crate) mod builtin_image;
pub(crate) mod builtin_tool;
pub(crate) mod mcp_self;
pub(crate) mod paths;
pub(crate) mod self_tool;
pub(crate) mod subagent;

/// Run the `outrig` command-line tool, returning the process exit code.
pub fn run() -> std::process::ExitCode {
    cli::app::run()
}
