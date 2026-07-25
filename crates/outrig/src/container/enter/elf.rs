// Pure ELF64 program-header parsing for the launcher.
//
// Just enough to decide whether a payload is statically linked or names a
// dynamic interpreter, and to reject anything that is not an ELF64. A shebang
// script would otherwise have its interpreter line resolve inside the *target*
// namespace and quietly mean something other than what the caller asked for,
// so we refuse it here rather than after the namespace switch.
//
// No syscalls and no I/O -- the caller reads the file. This file is the single
// source of truth: `launcher.rs` pulls it in with `include!`, so its header
// must be plain `//` comments (inner `//!` docs are illegal mid-file). The
// `outrig` crate also compiles it as `#[cfg(test)] mod elf` to run these tests
// on the host.

/// `PT_INTERP` program-header type.
const PT_INTERP: u32 = 3;
/// `EI_CLASS == ELFCLASS64`.
const ELFCLASS64: u8 = 2;
/// Fixed sizes of the ELF64 file header and one program-header entry.
const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;

/// What the payload's ELF header says about how to run it.
#[derive(Debug, PartialEq, Eq)]
enum ElfKind {
    /// No `PT_INTERP`: run straight from the fd; nothing resolves through the
    /// target container.
    Static,
    /// `PT_INTERP` present: the dynamic loader at this (sidecar-relative) path.
    Dynamic(String),
}

/// Why a payload was refused before any privileged work happened.
#[derive(Debug, PartialEq, Eq)]
enum ElfError {
    /// Missing ELF magic or not `ELFCLASS64` -- also how a shebang script (or
    /// any non-ELF64 file) is rejected.
    NotElf64,
    /// A header field or referenced offset runs past the bytes we read.
    Truncated,
}

impl std::fmt::Display for ElfError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotElf64 => f.write_str(
                "not an ELF64 binary (shebang scripts are not supported -- name the interpreter)",
            ),
            Self::Truncated => f.write_str("ELF header is truncated or malformed"),
        }
    }
}

/// Read a fixed-width field at `off`, or `Truncated` if it runs past `b`. The
/// caller picks the width via the `from_le_bytes` it feeds the result to.
fn rd<const N: usize>(b: &[u8], off: usize) -> Result<[u8; N], ElfError> {
    b.get(off..off + N)
        .and_then(|s| s.try_into().ok())
        .ok_or(ElfError::Truncated)
}

/// Classify `bytes` (the head of the payload file -- enough to cover the file
/// header, the program-header table, and any `PT_INTERP` string, which for
/// every real binary live at the very start). Little-endian only; both target
/// architectures (x86_64, aarch64) are little-endian.
fn elf_interp(bytes: &[u8]) -> Result<ElfKind, ElfError> {
    if bytes.len() < EHDR_LEN || bytes[..4] != *b"\x7fELF" || bytes[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    let e_phoff = u64::from_le_bytes(rd(bytes, 32)?) as usize;
    let e_phentsize = u16::from_le_bytes(rd(bytes, 54)?) as usize;
    let e_phnum = u16::from_le_bytes(rd(bytes, 56)?) as usize;
    if e_phentsize < PHDR_LEN {
        return Err(ElfError::Truncated);
    }
    for i in 0..e_phnum {
        let off = e_phoff
            .checked_add(i.checked_mul(e_phentsize).ok_or(ElfError::Truncated)?)
            .ok_or(ElfError::Truncated)?;
        let ph = bytes.get(off..off + PHDR_LEN).ok_or(ElfError::Truncated)?;
        if u32::from_le_bytes(rd(ph, 0)?) != PT_INTERP {
            continue;
        }
        let p_offset = u64::from_le_bytes(rd(ph, 8)?) as usize;
        let p_filesz = u64::from_le_bytes(rd(ph, 32)?) as usize;
        let end = p_offset.checked_add(p_filesz).ok_or(ElfError::Truncated)?;
        let raw = bytes.get(p_offset..end).ok_or(ElfError::Truncated)?;
        // The stored string is NUL-terminated; keep everything up to the first NUL.
        let s = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        return Ok(ElfKind::Dynamic(String::from_utf8_lossy(s).into_owned()));
    }
    Ok(ElfKind::Static)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal ELF64 head: 64-byte file header, then `interp`'s program
    /// headers packed at offset 64, then any interp strings after the table.
    /// `interps` is the list of `PT_INTERP` strings to emit (usually 0 or 1).
    fn elf64(interps: &[&str]) -> Vec<u8> {
        let phentsize = PHDR_LEN;
        let phnum = interps.len();
        let phoff = EHDR_LEN;
        let str_base = phoff + phnum * phentsize;

        let mut hdr = vec![0u8; EHDR_LEN];
        hdr[..4].copy_from_slice(b"\x7fELF");
        hdr[4] = ELFCLASS64;
        hdr[5] = 1; // EI_DATA = little-endian
        hdr[32..40].copy_from_slice(&(phoff as u64).to_le_bytes()); // e_phoff
        hdr[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes()); // e_phentsize
        hdr[56..58].copy_from_slice(&(phnum as u16).to_le_bytes()); // e_phnum

        // interp strings laid out contiguously after the PH table.
        let mut strtab = Vec::new();
        let mut offsets = Vec::new();
        for s in interps {
            offsets.push(str_base + strtab.len());
            strtab.extend_from_slice(s.as_bytes());
            strtab.push(0);
        }

        let mut phtab = Vec::new();
        for (idx, s) in interps.iter().enumerate() {
            let mut ph = vec![0u8; phentsize];
            ph[..4].copy_from_slice(&PT_INTERP.to_le_bytes()); // p_type
            ph[8..16].copy_from_slice(&(offsets[idx] as u64).to_le_bytes()); // p_offset
            ph[32..40].copy_from_slice(&((s.len() + 1) as u64).to_le_bytes()); // p_filesz (+NUL)
            phtab.extend_from_slice(&ph);
        }

        let mut out = hdr;
        out.extend_from_slice(&phtab);
        out.extend_from_slice(&strtab);
        out
    }

    #[test]
    fn detects_static() {
        assert_eq!(elf_interp(&elf64(&[])), Ok(ElfKind::Static));
    }

    #[test]
    fn detects_interp() {
        let bytes = elf64(&["/lib/ld-musl-x86_64.so.1"]);
        assert_eq!(
            elf_interp(&bytes),
            Ok(ElfKind::Dynamic("/lib/ld-musl-x86_64.so.1".to_string()))
        );
    }

    #[test]
    fn skips_non_interp_headers_then_finds_interp() {
        // A non-PT_INTERP header before the interp one must be walked past.
        let mut bytes = elf64(&["x", "/lib64/ld-linux-x86-64.so.2"]);
        // Rewrite the first header's p_type to PT_LOAD (1) so only the second counts.
        bytes[EHDR_LEN..EHDR_LEN + 4].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(
            elf_interp(&bytes),
            Ok(ElfKind::Dynamic("/lib64/ld-linux-x86-64.so.2".to_string()))
        );
    }

    #[test]
    fn rejects_shebang() {
        assert_eq!(
            elf_interp(b"#!/bin/sh\nexec node\n"),
            Err(ElfError::NotElf64)
        );
    }

    #[test]
    fn rejects_non_elf() {
        assert_eq!(
            elf_interp(b"MZ\x90\x00 not elf at all........"),
            Err(ElfError::NotElf64)
        );
        assert_eq!(elf_interp(b""), Err(ElfError::NotElf64));
    }

    #[test]
    fn rejects_elf32() {
        let mut bytes = elf64(&[]);
        bytes[4] = 1; // ELFCLASS32
        assert_eq!(elf_interp(&bytes), Err(ElfError::NotElf64));
    }

    #[test]
    fn truncated_program_header_table() {
        let mut bytes = elf64(&["/lib/ld.so"]);
        bytes.truncate(EHDR_LEN + 8); // header claims a PH entry the bytes don't contain
        assert_eq!(elf_interp(&bytes), Err(ElfError::Truncated));
    }
}
