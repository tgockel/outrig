//! Asserts that every `Field::doc_link` in the codebase points to a real
//! file under `doc/`. This catches drift between prompt help and the docs,
//! while still allowing optional mdBook anchors in the rendered URL.
//!
//! Field discovery is a manual `&[&Field]` slice. Each task that adds a
//! `Field` constant (0023 / 0024 / 0026) appends it to `ALL_FIELDS` below.
//! Today there are no caller-side fields yet, so the slice is seeded with
//! `EXAMPLE_FIELD` to exercise the validation path.

use std::path::Path;

use outrig_cli::init::prompt::Field;

static EXAMPLE_FIELD: Field = Field {
    name: "example",
    description: "An example field used to exercise the doc-link validation.",
    options: &[],
    doc_link: "doc/usage/init.md",
};

fn all_fields() -> Vec<&'static Field> {
    let mut v: Vec<&'static Field> = vec![&EXAMPLE_FIELD];
    v.extend(outrig_cli::config_init::DOC_SYNC_FIELDS.iter().copied());
    v.extend(outrig_cli::image_setup::DOC_SYNC_FIELDS.iter().copied());
    v.extend(outrig_cli::init::DOC_SYNC_FIELDS.iter().copied());
    v.extend(outrig_cli::init::repo::DOC_SYNC_FIELDS.iter().copied());
    v
}

#[test]
fn every_doc_link_resolves() {
    // doc_link is expressed relative to the workspace root (where `doc/`
    // lives); manifest dir for this crate is `crates/outrig-cli`.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    for field in all_fields() {
        let local_path = field
            .doc_link
            .split_once('#')
            .map_or(field.doc_link, |(path, _)| path);
        let path = workspace_root.join(local_path);
        assert!(
            path.is_file(),
            "Field {:?} has doc_link `{}` which does not resolve to a file (looked at {})",
            field.name,
            field.doc_link,
            path.display(),
        );
    }
}
