// Build-time verification of the static CPython archive every session mounts.
//
// `build.rs` pulls this file in with `include!`, beside
// `src/container/enter/elf.rs`, so its header must be plain `//` comments. The
// crate compiles it as `#[cfg(test)] mod archive` in `python/mod.rs` to run the
// tests below on the host; nothing in the library calls it.
//
// The digest is the gate. It is checked before a byte is decompressed, so an
// archive that is not the pinned one is refused without anything in it being
// read -- and the checks after it are about whether the pin itself is sound.

/// Lowercase hex SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The first of `dirs` holding `name` with the pinned digest. A download lands
/// in `OUT_DIR` when the per-user cache will not take it, so a rebuild has to
/// look in every place one can land before it reaches for the network.
fn cached_archive(dirs: &[&std::path::Path], name: &str, sha256: &str) -> Option<Vec<u8>> {
    dirs.iter().find_map(|dir| {
        std::fs::read(dir.join(name))
            .ok()
            .filter(|bytes| sha256_hex(bytes) == sha256)
    })
}

/// Refuse `archive` unless it is the pinned one and its `interpreter` member
/// is a static ELF64 for machine `e_machine` (62 for x86-64, 183 for AArch64).
fn verify_archive(
    archive: &[u8],
    sha256: &str,
    interpreter: &str,
    e_machine: u16,
) -> Result<(), String> {
    let actual = sha256_hex(archive);
    if actual != sha256 {
        return Err(format!("sha256 mismatch: expected {sha256}, actual {actual}"));
    }
    static_elf64(&member_head(archive, interpreter)?, e_machine)
        .map_err(|why| format!("{interpreter} is not usable: {why}"))
}

/// The first 64 KiB of `path` inside the tar.zst `archive` -- enough for the
/// ELF header and its program headers.
fn member_head(archive: &[u8], path: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;

    // No window limit: the only archives this decodes have already matched a
    // pinned digest, and the pinned one needs 128 MiB, past ruzstd's default.
    let decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(archive, u64::MAX)
        .map_err(|e| format!("not a zstd stream: {e}"))?;
    let mut tar = tar::Archive::new(decoder);
    let entries = tar.entries().map_err(|e| format!("not a tar: {e}"))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("reading the tar: {e}"))?;
        if entry.path().is_ok_and(|p| p == std::path::Path::new(path)) {
            let mut head = Vec::new();
            entry
                .take(64 * 1024)
                .read_to_end(&mut head)
                .map_err(|e| format!("reading {path}: {e}"))?;
            return Ok(head);
        }
    }
    Err(format!("the archive has no {path}"))
}

/// A statically linked ELF64 for `e_machine`: no `PT_INTERP`, so it runs in an
/// image with no loader and no libc. A static PIE qualifies.
fn static_elf64(head: &[u8], e_machine: u16) -> Result<(), String> {
    match elf_interp(head).map_err(|e| e.to_string())? {
        ElfKind::Dynamic(interp) => Err(format!("dynamically linked, against {interp}")),
        ElfKind::Static => match u16::from_le_bytes([head[18], head[19]]) {
            machine if machine == e_machine => Ok(()),
            machine => Err(format!("built for ELF machine {machine}, not {e_machine}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X86_64: u16 = 62;
    const AARCH64: u16 = 183;
    const INTERPRETER: &str = "python/install/bin/python3.13";

    /// An ELF64 header for `machine`, with one `PT_INTERP` naming `interp` if
    /// given. The same shape `enter/elf.rs`'s tests build.
    fn elf64(machine: u16, interp: Option<&str>) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 56];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2; // ELFCLASS64
        b[5] = 1; // little-endian
        b[18..20].copy_from_slice(&machine.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        b[56..58].copy_from_slice(&1u16.to_le_bytes()); // e_phnum
        if let Some(interp) = interp {
            let off = b.len() as u64;
            b[64..68].copy_from_slice(&3u32.to_le_bytes()); // PT_INTERP
            b[64 + 8..64 + 16].copy_from_slice(&off.to_le_bytes()); // p_offset
            b[64 + 32..64 + 40].copy_from_slice(&(interp.len() as u64 + 1).to_le_bytes());
            b.extend_from_slice(interp.as_bytes());
            b.push(0);
        }
        b
    }

    /// A tar.zst holding `members`, as the release archives are laid out.
    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, bytes) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            tar.append_data(&mut header, path, *bytes).unwrap();
        }
        let tar = tar.into_inner().unwrap();
        ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest)
    }

    fn verify(archive: &[u8], e_machine: u16) -> Result<(), String> {
        verify_archive(archive, &sha256_hex(archive), INTERPRETER, e_machine)
    }

    /// Bytes that are not even a zstd stream: had anything been decompressed
    /// before the digest check, the refusal would be a decode error instead.
    #[test]
    fn the_digest_is_checked_before_anything_is_decompressed() {
        let pinned = sha256_hex(b"the pinned archive");
        let err = verify_archive(b"a substitute", &pinned, INTERPRETER, X86_64).unwrap_err();
        assert!(err.starts_with("sha256 mismatch"), "{err}");
        assert!(err.contains(&format!("expected {pinned}")), "{err}");
        assert!(err.contains(&format!("actual {}", sha256_hex(b"a substitute"))), "{err}");
    }

    #[test]
    fn a_static_interpreter_for_the_target_is_accepted() {
        let elf = elf64(X86_64, None);
        verify(&archive(&[("python/build/junk", b"x"), (INTERPRETER, &elf)]), X86_64).unwrap();
        let elf = elf64(AARCH64, None);
        verify(&archive(&[(INTERPRETER, &elf)]), AARCH64).unwrap();
    }

    #[test]
    fn a_dynamically_linked_interpreter_is_refused() {
        let elf = elf64(X86_64, Some("/lib/ld-musl-x86_64.so.1"));
        let err = verify(&archive(&[(INTERPRETER, &elf)]), X86_64).unwrap_err();
        assert!(err.contains("dynamically linked, against /lib/ld-musl-x86_64.so.1"), "{err}");
    }

    #[test]
    fn an_interpreter_for_another_machine_is_refused() {
        let elf = elf64(AARCH64, None);
        let err = verify(&archive(&[(INTERPRETER, &elf)]), X86_64).unwrap_err();
        assert!(err.contains("ELF machine 183, not 62"), "{err}");
    }

    #[test]
    fn an_elf32_or_a_script_is_refused() {
        let mut elf32 = elf64(X86_64, None);
        elf32[4] = 1; // ELFCLASS32
        for member in [&elf32[..], b"#!/bin/sh\n"] {
            let err = verify(&archive(&[(INTERPRETER, member)]), X86_64).unwrap_err();
            assert!(err.contains("not an ELF64"), "{err}");
        }
    }

    /// A download that fell back to `OUT_DIR` is found there on a rebuild, and a
    /// copy that does not match the pin is passed over wherever it sits.
    #[test]
    fn a_cached_archive_is_found_in_any_place_a_download_lands() {
        let (cache, out_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let dirs = [cache.path(), out_dir.path()];
        let pinned = sha256_hex(b"pinned");

        assert_eq!(cached_archive(&dirs, "a.tar.zst", &pinned), None);
        std::fs::write(out_dir.path().join("a.tar.zst"), b"pinned").unwrap();
        assert_eq!(
            cached_archive(&dirs, "a.tar.zst", &pinned).as_deref(),
            Some(&b"pinned"[..])
        );
        std::fs::write(cache.path().join("a.tar.zst"), b"truncated").unwrap();
        assert_eq!(
            cached_archive(&dirs, "a.tar.zst", &pinned).as_deref(),
            Some(&b"pinned"[..])
        );
    }

    #[test]
    fn an_archive_without_the_interpreter_is_refused() {
        let err = verify(&archive(&[("python/install/README", b"x")]), X86_64).unwrap_err();
        assert!(err.contains(&format!("the archive has no {INTERPRETER}")), "{err}");
    }
}
