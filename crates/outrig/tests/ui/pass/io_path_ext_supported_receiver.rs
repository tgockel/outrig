//! The supported receiver still works, from outside the crate. trybuild both
//! compiles and runs this, so `path_ctx` is called rather than only named.
//!
//! It asserts nothing about the rendering: `OutrigError::Path`'s wording is
//! owned by `path_error_names_the_path_and_operation` in `src/error.rs`, and a
//! second copy here would cost a scratch-project rebuild to re-run every time
//! that wording moved.

use outrig::error::IoPathExt;

fn main() {
    let result: Result<(), std::io::Error> = Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    result.path_ctx("open", "/tmp/nope").unwrap_err();
}
