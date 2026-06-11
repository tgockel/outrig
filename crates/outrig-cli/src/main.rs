#![doc = include_str!("../README.md")]

use std::process::ExitCode;

fn main() -> ExitCode {
    outrig_cli::cli::app::run()
}
