//! Scaffolding for new image-configs: `outrig image add`
//! generates a Dockerfile plus a `[images.<name>]` block by rendering
//! a curated set of base-image templates.

pub mod add;
pub mod build;
pub mod init;
pub mod render;

pub use add::DOC_SYNC_FIELDS;
