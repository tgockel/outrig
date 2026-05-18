//! Scaffolding for new container-configs: `outrig container add`
//! generates a Dockerfile plus a `[containers.<name>]` block by rendering
//! a curated set of base-image templates.

pub mod add;
pub mod render;

pub use add::DOC_SYNC_FIELDS;
