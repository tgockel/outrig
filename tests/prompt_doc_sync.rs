//! Asserts that every `Field::doc_link` in the codebase points to a real
//! file under `doc/`. This catches drift between prompt help and the docs.
//!
//! Field discovery is a manual `&[&Field]` slice. Each task that adds a
//! `Field` constant (0023 / 0024 / 0026) appends it to `ALL_FIELDS` below.
//! Today there are no caller-side fields yet, so the slice is seeded with
//! `EXAMPLE_FIELD` to exercise the validation path.

use std::path::Path;

use outrig::init::prompt::Field;

static EXAMPLE_FIELD: Field = Field {
    name: "example",
    description: "An example field used to exercise the doc-link validation.",
    options: &[],
    doc_link: "doc/usage/init.md",
};

static ALL_FIELDS: &[&Field] = &[&EXAMPLE_FIELD];

#[test]
fn every_doc_link_resolves() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    for field in ALL_FIELDS {
        let path = Path::new(manifest_dir).join(field.doc_link);
        assert!(
            path.is_file(),
            "Field {:?} has doc_link `{}` which does not resolve to a file (looked at {})",
            field.name,
            field.doc_link,
            path.display(),
        );
    }
}
