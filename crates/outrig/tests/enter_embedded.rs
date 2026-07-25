//! The embedded `outrig-enter` launcher: static-ELF shape and materialization.
//!
//! Container-level behavior (setns, graft, MCP handshake, the SYS_ADMIN /
//! SYS_PTRACE negative cases) needs a running OutRig session and podman, so it
//! is exercised by task 0090 and by hand against a live session -- see the
//! prototype's `21-sidecar-setns.sh` / `22-sidecar-mcp-demo.sh`.

use std::os::unix::fs::PermissionsExt;

use outrig::container::enter;

/// `PT_INTERP` present? A statically linked binary (including static-pie) has
/// none; a dynamically linked one names its loader here.
fn has_pt_interp(b: &[u8]) -> bool {
    const PT_INTERP: u32 = 3;
    let rd_u16 = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
    let rd_u64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let phoff = rd_u64(32) as usize;
    let phentsize = rd_u16(54) as usize;
    let phnum = rd_u16(56) as usize;
    (0..phnum).any(|i| {
        let off = phoff + i * phentsize;
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap()) == PT_INTERP
    })
}

#[test]
fn embedded_helper_is_static_elf64_and_materializes_0755() {
    if !enter::is_available() {
        // Built without the musl target: materialize must fail with the
        // actionable "built without the helper" message, not write a stub.
        let dir = tempfile::tempdir().unwrap();
        let err = enter::materialize(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("filesystem-view helper"),
            "unexpected error: {err}"
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let path = enter::materialize(dir.path()).unwrap();
    assert_eq!(path, dir.path().join("outrig-enter"));

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 4, "embedded launcher is suspiciously small");
    assert_eq!(&bytes[..4], b"\x7fELF", "not an ELF binary");
    assert_eq!(bytes[4], 2, "expected ELFCLASS64");
    assert!(
        !has_pt_interp(&bytes),
        "embedded launcher must be statically linked (no PT_INTERP)"
    );

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "mode was {mode:o}");

    // The write is deterministic: a second materialization is byte-identical.
    let dir2 = tempfile::tempdir().unwrap();
    let path2 = enter::materialize(dir2.path()).unwrap();
    assert_eq!(bytes, std::fs::read(&path2).unwrap());
}
