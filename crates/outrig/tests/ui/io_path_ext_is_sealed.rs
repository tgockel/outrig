//! `Local` is defined right here, so the orphan rules permit this impl and the
//! private supertrait is the only thing that can reject it. The golden
//! `.stderr` beside this file is what says so.

use std::path::PathBuf;

struct Local;

impl outrig::error::IoPathExt<()> for Local {
    fn path_ctx(self, _op: &'static str, _path: impl Into<PathBuf>) -> outrig::error::Result<()> {
        unimplemented!()
    }
}

fn main() {}
