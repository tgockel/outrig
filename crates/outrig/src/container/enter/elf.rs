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
// crate's `build.rs` includes it too, as `mod elf`, to check that its fallback
// build of the launcher came out static, and the `outrig` crate compiles it as
// `#[cfg(test)] mod elf` to run these tests on the host.

/// `PT_LOAD`, `PT_DYNAMIC` and `PT_INTERP` program-header types.
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
/// Dynamic-section tags `elf_needed` reads.
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_STRTAB: u64 = 5;
const DT_SONAME: u64 = 14;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;
/// `EI_CLASS == ELFCLASS64`.
const ELFCLASS64: u8 = 2;
/// Fixed sizes of the ELF64 file header and one program-header entry.
const EHDR_LEN: usize = 64;
const PHDR_LEN: usize = 56;

/// What the payload's ELF header says about how to run it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ElfKind {
    /// No `PT_INTERP`: run straight from the fd; nothing resolves through the
    /// target container.
    Static,
    /// `PT_INTERP` present: the dynamic loader at this (sidecar-relative) path.
    Dynamic(String),
}

/// Why a payload was refused before any privileged work happened.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ElfError {
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
pub(crate) fn elf_interp(bytes: &[u8]) -> Result<ElfKind, ElfError> {
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

/// What an object's dynamic section asks the loader to find.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Needed {
    /// Each `DT_NEEDED` name, in order. Bytes: a file name need not be UTF-8,
    /// and the loader looks up the bytes the object holds.
    pub(crate) names: Vec<Vec<u8>>,
    /// Each `DT_RPATH` value, unsplit.
    pub(crate) rpath: Vec<Vec<u8>>,
    /// Each `DT_RUNPATH` value, unsplit.
    pub(crate) runpath: Vec<Vec<u8>>,
    /// The object's own `DT_SONAME`, if it has one: a name the loader treats
    /// as already satisfied once the object is loaded.
    pub(crate) soname: Vec<Vec<u8>>,
}

/// The `DT_NEEDED` names and `DT_RPATH` / `DT_RUNPATH` values of the ELF64 in
/// `bytes`, which must be the whole file: unlike `elf_interp`, the string table
/// can sit anywhere in it. An object with no `PT_DYNAMIC` needs nothing.
pub(crate) fn elf_needed(bytes: &[u8]) -> Result<Needed, ElfError> {
    if bytes.len() < EHDR_LEN || bytes[..4] != *b"\x7fELF" || bytes[4] != ELFCLASS64 {
        return Err(ElfError::NotElf64);
    }
    let e_phoff = u64::from_le_bytes(rd(bytes, 32)?) as usize;
    let e_phentsize = u16::from_le_bytes(rd(bytes, 54)?) as usize;
    let e_phnum = u16::from_le_bytes(rd(bytes, 56)?) as usize;
    if e_phentsize < PHDR_LEN {
        return Err(ElfError::Truncated);
    }
    // (p_offset, p_vaddr, p_filesz) of each PT_LOAD, and the dynamic segment's
    // (p_offset, p_filesz).
    let mut loads: Vec<(u64, u64, u64)> = Vec::new();
    let mut dynamic: Option<(u64, u64)> = None;
    for i in 0..e_phnum {
        let off = e_phoff
            .checked_add(i.checked_mul(e_phentsize).ok_or(ElfError::Truncated)?)
            .ok_or(ElfError::Truncated)?;
        let ph = bytes.get(off..off + PHDR_LEN).ok_or(ElfError::Truncated)?;
        let (p_offset, p_vaddr, p_filesz) = (
            u64::from_le_bytes(rd(ph, 8)?),
            u64::from_le_bytes(rd(ph, 16)?),
            u64::from_le_bytes(rd(ph, 32)?),
        );
        match u32::from_le_bytes(rd(ph, 0)?) {
            PT_LOAD => loads.push((p_offset, p_vaddr, p_filesz)),
            PT_DYNAMIC => dynamic = Some((p_offset, p_filesz)),
            _ => {}
        }
    }
    let Some((dyn_off, dyn_len)) = dynamic else {
        return Ok(Needed::default());
    };
    let dyn_off = usize::try_from(dyn_off).map_err(|_| ElfError::Truncated)?;
    let dyn_len = usize::try_from(dyn_len).map_err(|_| ElfError::Truncated)?;
    let section = bytes
        .get(dyn_off..dyn_off.checked_add(dyn_len).ok_or(ElfError::Truncated)?)
        .ok_or(ElfError::Truncated)?;

    // Tag/value pairs up to DT_NULL; strings are offsets into the table
    // `DT_STRTAB` names by virtual address, so collect before resolving.
    let mut strtab: Option<u64> = None;
    let mut needed: Vec<u64> = Vec::new();
    let mut rpath: Vec<u64> = Vec::new();
    let mut runpath: Vec<u64> = Vec::new();
    let mut soname: Vec<u64> = Vec::new();
    for entry in section.as_chunks::<16>().0 {
        let tag = u64::from_le_bytes(rd(entry, 0)?);
        let val = u64::from_le_bytes(rd(entry, 8)?);
        match tag {
            DT_NULL => break,
            DT_NEEDED => needed.push(val),
            DT_STRTAB => strtab = Some(val),
            DT_RPATH => rpath.push(val),
            DT_RUNPATH => runpath.push(val),
            DT_SONAME => soname.push(val),
            _ => {}
        }
    }
    if needed.is_empty() && rpath.is_empty() && runpath.is_empty() && soname.is_empty() {
        return Ok(Needed::default());
    }
    let strtab = strtab.ok_or(ElfError::Truncated)?;
    let base = loads
        .iter()
        .find(|&&(_, vaddr, filesz)| strtab >= vaddr && strtab - vaddr < filesz)
        .map(|&(offset, vaddr, _)| offset + (strtab - vaddr))
        .ok_or(ElfError::Truncated)?;
    let base = usize::try_from(base).map_err(|_| ElfError::Truncated)?;
    let string = |at: u64| -> Result<Vec<u8>, ElfError> {
        let start = base
            .checked_add(usize::try_from(at).map_err(|_| ElfError::Truncated)?)
            .ok_or(ElfError::Truncated)?;
        let raw = bytes.get(start..).ok_or(ElfError::Truncated)?;
        let end = raw
            .iter()
            .position(|&b| b == 0)
            .ok_or(ElfError::Truncated)?;
        Ok(raw[..end].to_vec())
    };
    Ok(Needed {
        names: needed.into_iter().map(string).collect::<Result<_, _>>()?,
        rpath: rpath.into_iter().map(string).collect::<Result<_, _>>()?,
        runpath: runpath.into_iter().map(string).collect::<Result<_, _>>()?,
        soname: soname.into_iter().map(string).collect::<Result<_, _>>()?,
    })
}

/// `e_machine` of `bytes` if it opens with an ELF64 header, from the header
/// alone: a candidate's program headers are not looked at until it is known to
/// be one the loader would consider.
pub(crate) fn elf64_machine(bytes: &[u8]) -> Option<u16> {
    (bytes.len() >= EHDR_LEN && bytes[..4] == *b"\x7fELF" && bytes[4] == ELFCLASS64)
        .then(|| u16::from_le_bytes([bytes[18], bytes[19]]))
}

/// An ELF64 whose single `PT_LOAD` maps the whole file at vaddr 0 and whose
/// `PT_DYNAMIC` lists `needed` as `DT_NEEDED`, `rpath` as `DT_RPATH` and
/// `runpath` as `DT_RUNPATH`.
#[cfg(test)]
pub(crate) fn elf64_dynamic(
    needed: &[&str],
    rpath: Option<&str>,
    runpath: Option<&str>,
) -> Vec<u8> {
    elf64_dynamic_soname(needed, rpath, runpath, None)
}

/// [`elf64_dynamic`] with a `DT_SONAME`.
#[cfg(test)]
pub(crate) fn elf64_dynamic_soname(
    needed: &[&str],
    rpath: Option<&str>,
    runpath: Option<&str>,
    soname: Option<&str>,
) -> Vec<u8> {
    let mut strtab = vec![0u8];
    let mut add = |s: &str| {
        let at = strtab.len() as u64;
        strtab.extend_from_slice(s.as_bytes());
        strtab.push(0);
        at
    };
    let mut entries: Vec<(u64, u64)> = needed.iter().map(|n| (DT_NEEDED, add(n))).collect();
    if let Some(r) = soname {
        entries.push((DT_SONAME, add(r)));
    }
    if let Some(r) = rpath {
        entries.push((DT_RPATH, add(r)));
    }
    if let Some(r) = runpath {
        entries.push((DT_RUNPATH, add(r)));
    }
    let phoff = EHDR_LEN;
    let dyn_off = phoff + 2 * PHDR_LEN;
    let dyn_len = (entries.len() + 2) * 16;
    let str_off = dyn_off + dyn_len;
    entries.push((DT_STRTAB, str_off as u64));
    entries.push((DT_NULL, 0));
    let total = str_off + strtab.len();

    let mut out = vec![0u8; EHDR_LEN];
    out[..4].copy_from_slice(b"\x7fELF");
    out[4] = ELFCLASS64;
    out[5] = 1;
    out[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
    out[54..56].copy_from_slice(&(PHDR_LEN as u16).to_le_bytes());
    out[56..58].copy_from_slice(&2u16.to_le_bytes());
    let phdr = |ty: u32, off: usize, len: usize| {
        let mut ph = vec![0u8; PHDR_LEN];
        ph[..4].copy_from_slice(&ty.to_le_bytes());
        ph[8..16].copy_from_slice(&(off as u64).to_le_bytes());
        ph[16..24].copy_from_slice(&(off as u64).to_le_bytes()); // vaddr == offset
        ph[32..40].copy_from_slice(&(len as u64).to_le_bytes());
        ph
    };
    out.extend(phdr(PT_LOAD, 0, total));
    out.extend(phdr(PT_DYNAMIC, dyn_off, dyn_len));
    for (tag, val) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&val.to_le_bytes());
    }
    out.extend_from_slice(&strtab);
    out
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

    #[test]
    fn needed_names_and_runpath() {
        let bytes = elf64_dynamic(
            &["liblzma.so.5", "libc.so.6"],
            Some("/a"),
            Some("$ORIGIN/../lib"),
        );
        assert_eq!(
            elf_needed(&bytes),
            Ok(Needed {
                names: vec![b"liblzma.so.5".to_vec(), b"libc.so.6".to_vec()],
                rpath: vec![b"/a".to_vec()],
                runpath: vec![b"$ORIGIN/../lib".to_vec()],
                soname: vec![],
            })
        );
    }

    #[test]
    fn no_dynamic_section_needs_nothing() {
        assert_eq!(elf_needed(&elf64(&[])), Ok(Needed::default()));
    }

    #[test]
    fn needed_rejects_non_elf_and_truncation() {
        assert_eq!(elf_needed(b"#!/bin/sh\n"), Err(ElfError::NotElf64));
        let mut bytes = elf64_dynamic(&["liblzma.so.5"], None, None);
        bytes.truncate(bytes.len() - 4); // cuts the string table mid-name
        assert_eq!(elf_needed(&bytes), Err(ElfError::Truncated));
    }
}
