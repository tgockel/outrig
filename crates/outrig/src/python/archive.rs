// Build-time verification of what the Python side embeds: the static CPython
// archive every session mounts, and the pure-Python wheels unpacked beside it.
//
// `build.rs` pulls this file in with `include!`, beside
// `src/container/enter/elf.rs`, so its header must be plain `//` comments. The
// crate compiles it as `#[cfg(test)] mod archive` in `python/mod.rs` to run the
// tests below on the host; nothing in the library calls it.
//
// The digest is the gate. It is checked before a byte is decompressed or
// parsed, so an archive that is not the pinned one is refused without anything
// in it being read -- and the checks after it are about whether the pin itself
// is sound.

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
    verify_digest(archive, sha256)?;
    let head = member_head(archive, interpreter)?;
    static_elf64(&head, e_machine)
        .map_err(|why| format!("{interpreter} is not usable: {why}"))?;
    stack_field(&head).map(|_| ()).map_err(|why| {
        format!(
            "{interpreter} cannot receive the {} MiB thread-stack patch: {why}",
            THREAD_STACK >> 20
        )
    })
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

/// Refuse `bytes` unless their digest is the pinned one. For a wheel this is
/// the whole check: a pure-Python wheel holds source, and the digest says whose.
fn verify_digest(bytes: &[u8], sha256: &str) -> Result<(), String> {
    let actual = sha256_hex(bytes);
    if actual == sha256 {
        Ok(())
    } else {
        Err(format!("sha256 mismatch: expected {sha256}, actual {actual}"))
    }
}

/// `wheel`'s members as an uncompressed tar, each a regular file under
/// `prefix/` with its Unix mode (0644 when the zip records none), so the
/// runtime unpacks a wheel exactly as it unpacks the payload.
///
/// A reader for what the pinned wheels are, not for zip in general: one
/// archive with no zip64 fields, data descriptors, directory entries or
/// encryption, whose members are stored or deflated. Anything else is an
/// error -- a wrong pin, not a missing one -- and so is a member that would
/// land outside its tree. The bytes are the pinned wheel's, so CRCs go
/// unchecked; a member that does not inflate to its declared size is refused.
fn wheel_to_tar(wheel: &[u8], prefix: &str) -> Result<Vec<u8>, String> {
    // The end record is the last 22 bytes, or earlier by the length of a comment.
    let end = (0..wheel.len().saturating_sub(21))
        .rev()
        .find(|&at| wheel[at..].starts_with(&END_RECORD))
        .ok_or("no end-of-central-directory record")?;
    if le(wheel, end + 4, 4)? != 0 {
        return Err("spans more than one disk".into());
    }
    let count = le(wheel, end + 10, 2)?;
    let mut at = le(wheel, end + 16, 4)?;
    if count == 0xFFFF || at == 0xFFFF_FFFF {
        return Err("a zip64 archive".into());
    }

    let mut tar = tar::Builder::new(Vec::new());
    for _ in 0..count {
        if wheel.get(at..at + 4) != Some(&CENTRAL_HEADER[..]) {
            return Err(format!("no central directory entry at offset {at}"));
        }
        let flags = le(wheel, at + 8, 2)?;
        let method = le(wheel, at + 10, 2)?;
        let compressed = le(wheel, at + 20, 4)?;
        let size = le(wheel, at + 24, 4)?;
        let name_len = le(wheel, at + 28, 2)?;
        let mode = (le(wheel, at + 38, 4)? >> 16) & 0o777;
        let local = le(wheel, at + 42, 4)?;
        let name = wheel
            .get(at + 46..at + 46 + name_len)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or_else(|| format!("the member name at offset {at} is not UTF-8"))?;
        at += 46 + name_len + le(wheel, at + 30, 2)? + le(wheel, at + 32, 2)?;

        if name.contains('\\') || name.split('/').any(|part| part.is_empty() || part == "..") {
            return Err(format!("member {name:?} is not a plain relative file path"));
        }
        // Bit 0 is encryption, bit 3 a data descriptor after the data.
        if flags & 0b1001 != 0 {
            return Err(format!("{name} is encrypted or has a data descriptor"));
        }
        if compressed == 0xFFFF_FFFF || size == 0xFFFF_FFFF {
            return Err(format!("{name} has zip64 sizes"));
        }
        if wheel.get(local..local + 4) != Some(&LOCAL_HEADER[..]) {
            return Err(format!("{name} has no local header at offset {local}"));
        }
        let start = local + 30 + le(wheel, local + 26, 2)? + le(wheel, local + 28, 2)?;
        let stored = wheel
            .get(start..start + compressed)
            .ok_or_else(|| format!("{name} is truncated"))?;
        let data = match method {
            0 => std::borrow::Cow::Borrowed(stored),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(stored, size)
                .map_err(|e| format!("inflating {name}: {e}"))?
                .into(),
            other => return Err(format!("{name} uses compression method {other}")),
        };
        if data.len() != size {
            return Err(format!("{name} inflated to {} bytes, not {size}", data.len()));
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(size as u64);
        header.set_mode(if mode == 0 { 0o644 } else { mode as u32 });
        tar.append_data(&mut header, format!("{prefix}/{name}"), &*data)
            .map_err(|e| format!("adding {name} to the tar: {e}"))?;
    }
    tar.into_inner()
        .map_err(|e| format!("finishing the tar: {e}"))
}

const LOCAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const CENTRAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const END_RECORD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];

/// The little-endian integer of `len` bytes at `at`, or an error for a zip
/// cut short.
fn le(bytes: &[u8], at: usize, len: usize) -> Result<usize, String> {
    bytes
        .get(at..at + len)
        .map(|field| field.iter().rev().fold(0, |n, &b| (n << 8) | usize::from(b)))
        .ok_or_else(|| format!("truncated at offset {at}"))
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
        b[64..68].copy_from_slice(&0x6474e551u32.to_le_bytes()); // PT_GNU_STACK
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

    // ------------------------------------------------------------------ wheels

    /// One member of a synthetic wheel: deflated, mode 644, unless said otherwise.
    struct M {
        name: &'static str,
        data: &'static [u8],
        mode: u32,
        method: u16,
        flags: u16,
    }

    impl Default for M {
        fn default() -> Self {
            Self {
                name: "",
                data: b"",
                mode: 0o644,
                method: 8,
                flags: 0,
            }
        }
    }

    fn m(name: &'static str, data: &'static [u8]) -> M {
        M {
            name,
            data,
            ..M::default()
        }
    }

    /// A zip as wheel tooling writes one: local headers, a central directory,
    /// and the end record, with no extra fields and no comments.
    fn zip(members: &[M]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for member in members {
            let stored = match member.method {
                8 => miniz_oxide::deflate::compress_to_vec(member.data, 6),
                _ => member.data.to_vec(),
            };
            let offset = out.len() as u32;
            out.extend_from_slice(&LOCAL_HEADER);
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&member.flags.to_le_bytes());
            out.extend_from_slice(&member.method.to_le_bytes());
            out.extend_from_slice(&[0; 8]); // time, date, crc (unchecked)
            out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&(member.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(member.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(member.name.as_bytes());
            out.extend_from_slice(&stored);

            central.extend_from_slice(&CENTRAL_HEADER);
            central.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes()); // made on Unix
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&member.flags.to_le_bytes());
            central.extend_from_slice(&member.method.to_le_bytes());
            central.extend_from_slice(&[0; 8]); // time, date, crc
            central.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            central.extend_from_slice(&(member.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(member.name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 8]); // extra, comment, disk, internal attrs
            central.extend_from_slice(&(member.mode << 16).to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(member.name.as_bytes());
        }
        let directory = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&END_RECORD);
        out.extend_from_slice(&[0; 4]); // disks
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out
    }

    /// `(path, mode, bytes)` of every member of `tar`.
    fn members(tar: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
        use std::io::Read;
        let mut archive = tar::Archive::new(tar);
        let mut found = Vec::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mode = entry.header().mode().unwrap();
            let mut data = Vec::new();
            entry.read_to_end(&mut data).unwrap();
            found.push((path, mode, data));
        }
        found
    }

    #[test]
    fn a_wheel_that_is_not_the_pinned_one_is_refused() {
        let wheel = zip(&[m("rpyc/__init__.py", b"")]);
        verify_digest(&wheel, &sha256_hex(&wheel)).unwrap();
        let err = verify_digest(b"a substitute", &sha256_hex(&wheel)).unwrap_err();
        assert!(err.starts_with("sha256 mismatch"), "{err}");
    }

    #[test]
    fn stored_and_deflated_members_land_under_the_pin_with_their_modes() {
        let wheel = zip(&[
            m("rpyc/__init__.py", b"from rpyc.core import Connection\n"),
            M {
                method: 0,
                ..m("rpyc-6.0.2.dist-info/WHEEL", b"Wheel-Version: 1.0\n")
            },
            M {
                mode: 0o755,
                ..m("rpyc-6.0.2.data/scripts/tool", b"#!/bin/sh\n")
            },
        ]);
        let tar = wheel_to_tar(&wheel, "rpyc-6.0.2-py3-none-any").unwrap();
        assert_eq!(
            members(&tar),
            [
                (
                    "rpyc-6.0.2-py3-none-any/rpyc/__init__.py".to_string(),
                    0o644,
                    b"from rpyc.core import Connection\n".to_vec()
                ),
                (
                    "rpyc-6.0.2-py3-none-any/rpyc-6.0.2.dist-info/WHEEL".to_string(),
                    0o644,
                    b"Wheel-Version: 1.0\n".to_vec()
                ),
                (
                    "rpyc-6.0.2-py3-none-any/rpyc-6.0.2.data/scripts/tool".to_string(),
                    0o755,
                    b"#!/bin/sh\n".to_vec()
                ),
            ]
        );
    }

    #[test]
    fn bytes_that_are_not_a_zip_are_refused() {
        let err = wheel_to_tar(b"not a zip at all, and longer than a record", "p").unwrap_err();
        assert!(err.contains("no end-of-central-directory record"), "{err}");
        let err = wheel_to_tar(b"short", "p").unwrap_err();
        assert!(err.contains("no end-of-central-directory record"), "{err}");
    }

    #[test]
    fn a_member_that_would_land_outside_its_tree_is_refused() {
        for name in ["../escape", "/etc/passwd", "a/../b", "dir/", "", "back\\slash"] {
            let err = wheel_to_tar(&zip(&[m(name, b"x")]), "p").unwrap_err();
            assert!(err.contains("not a plain relative file path"), "{name}: {err}");
        }
    }

    #[test]
    fn a_member_the_reader_does_not_handle_is_refused() {
        let err = wheel_to_tar(
            &zip(&[M {
                method: 12,
                ..m("rpyc/x.py", b"x")
            }]),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("compression method 12"), "{err}");
        for flags in [1, 8] {
            let err = wheel_to_tar(&zip(&[M { flags, ..m("rpyc/x.py", b"x") }]), "p").unwrap_err();
            assert!(err.contains("encrypted or has a data descriptor"), "{err}");
        }
        // Stored bytes that are not a deflate stream.
        let mut wheel = zip(&[m("rpyc/x.py", b"some source text here")]);
        wheel[30 + "rpyc/x.py".len()] ^= 0xff;
        let err = wheel_to_tar(&wheel, "p").unwrap_err();
        assert!(err.contains("inflating rpyc/x.py"), "{err}");
    }
}
