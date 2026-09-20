//! The seal has to bound the trait's type argument, not only `Self`.
//!
//! `Result<(), io::Error>` is a supported receiver, so it satisfies a seal
//! written on `Self` alone. A local type as the *argument* then satisfies the
//! orphan rule and cannot overlap `impl<T> IoPathExt<T> for Result<T, _>`,
//! because that one requires the argument to equal the `Ok` type.

use std::path::PathBuf;

struct Local;

impl outrig::error::IoPathExt<Local> for Result<(), std::io::Error> {
    fn path_ctx(
        self,
        _op: &'static str,
        _path: impl Into<PathBuf>,
    ) -> outrig::error::Result<Local> {
        unimplemented!()
    }
}

fn main() {}
