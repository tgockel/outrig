// A reader for the zip archives Python's wheels are, shared by `build.rs`,
// which converts the pinned wheels it embeds, and the library, which reads an
// owner's application wheel (`harness/application.rs`).
//
// `build.rs` pulls this file in with `include!`, beside `archive.rs`, so its
// header must be plain `//` comments and it reaches other crates by path and
// nothing of the crate's. The crate compiles it as the module `python::zip`.
//
// It reads what wheel tooling writes, not zip in general: one archive with no
// zip64 fields and no encryption, whose members are stored or deflated.
// Anything else is refused. Sizes, CRCs and modes come from the central
// directory, which is authoritative, so a member followed by a data descriptor
// reads as any other.

/// `zip`'s files as an uncompressed tar, in the zip's order. Each member is
/// put to `place` with its name and the size the zip declares for it before
/// any of it is inflated: `Ok(Some(path))` adds it at `path` with its Unix mode
/// (0644 when the zip records none), `Ok(None)` leaves it out unread, and an
/// error refuses the archive. A directory entry is no file and adds nothing. A
/// member that would land outside its tree is refused, and so is one the zip
/// marks as a link or as anything else that is not a regular file, one that
/// does not inflate to its declared size, and one whose CRC-32 is not the one
/// declared.
pub(crate) fn zip_to_tar(
    zip: &[u8],
    mut place: impl FnMut(&str, u64) -> Result<Option<String>, String>,
) -> Result<Vec<u8>, String> {
    // The end record is the last 22 bytes, or earlier by the length of a comment,
    // which is at most 65,535 bytes long.
    let end = (zip.len().saturating_sub(22 + 0xFFFF)..zip.len().saturating_sub(21))
        .rev()
        .find(|&at| zip[at..].starts_with(&END_RECORD))
        .ok_or("no end-of-central-directory record")?;
    if le(zip, end + 4, 4)? != 0 {
        return Err("spans more than one disk".into());
    }
    let count = le(zip, end + 10, 2)?;
    let mut at = le(zip, end + 16, 4)?;
    if count == 0xFFFF || at == 0xFFFF_FFFF {
        return Err("a zip64 archive".into());
    }

    let mut tar = tar::Builder::new(Vec::new());
    for _ in 0..count {
        if zip.get(at..at + 4) != Some(&CENTRAL_HEADER[..]) {
            return Err(format!("no central directory entry at offset {at}"));
        }
        let flags = le(zip, at + 8, 2)?;
        let method = le(zip, at + 10, 2)?;
        let crc = le(zip, at + 16, 4)?;
        let compressed = le(zip, at + 20, 4)?;
        let size = le(zip, at + 24, 4)?;
        let name_len = le(zip, at + 28, 2)?;
        let unix = le(zip, at + 38, 4)? >> 16;
        let local = le(zip, at + 42, 4)?;
        let name = zip
            .get(at + 46..at + 46 + name_len)
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .ok_or_else(|| format!("the member name at offset {at} is not UTF-8"))?;
        at += 46 + name_len + le(zip, at + 30, 2)? + le(zip, at + 32, 2)?;

        let (path, directory) = match name.strip_suffix('/') {
            Some(path) => (path, true),
            None => (name, false),
        };
        if path.contains('\\') || path.split('/').any(|part| matches!(part, "" | "." | "..")) {
            return Err(format!("member {name:?} is not a plain relative file path"));
        }
        if directory {
            continue;
        }
        let Some(placed) = place(name, size as u64)? else {
            continue;
        };
        // Bit 0 is encryption. Bit 3, a data descriptor after the data, says
        // only that the local header lacks what the central directory has.
        if flags & 1 != 0 {
            return Err(format!("{name} is encrypted"));
        }
        match unix & 0o170000 {
            0 | 0o100000 => {}
            0o120000 => return Err(format!("{name} is a symbolic link, which is not followed")),
            kind => {
                return Err(format!(
                    "{name} is not a regular file (its type is {kind:o})"
                ));
            }
        }
        if compressed == 0xFFFF_FFFF || size == 0xFFFF_FFFF {
            return Err(format!("{name} has zip64 sizes"));
        }
        if zip.get(local..local + 4) != Some(&LOCAL_HEADER[..]) {
            return Err(format!("{name} has no local header at offset {local}"));
        }
        let start = local + 30 + le(zip, local + 26, 2)? + le(zip, local + 28, 2)?;
        let stored = zip
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
            return Err(format!(
                "{name} inflated to {} bytes, not {size}",
                data.len()
            ));
        }
        if crc32(&data) as usize != crc {
            return Err(format!("{name} does not match its CRC-32"));
        }
        let mode = unix & 0o777;
        let mut header = tar::Header::new_gnu();
        header.set_size(size as u64);
        header.set_mode(if mode == 0 { 0o644 } else { mode as u32 });
        tar.append_data(&mut header, &placed, &*data)
            .map_err(|e| format!("adding {name} to the tar: {e}"))?;
    }
    tar.into_inner()
        .map_err(|e| format!("finishing the tar: {e}"))
}

const LOCAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
pub(crate) const CENTRAL_HEADER: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const END_RECORD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];

/// The little-endian integer of `len` bytes at `at`, or an error for a zip
/// cut short.
fn le(bytes: &[u8], at: usize, len: usize) -> Result<usize, String> {
    bytes
        .get(at..at + len)
        .map(|field| {
            field
                .iter()
                .rev()
                .fold(0, |n, &b| (n << 8) | usize::from(b))
        })
        .ok_or_else(|| format!("truncated at offset {at}"))
}

/// The CRC-32 zip records of `bytes`: IEEE 802.3's, reflected.
fn crc32(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(!0u32, |crc, &byte| {
        CRC_TABLE[usize::from((crc as u8) ^ byte)] ^ (crc >> 8)
    })
}

/// [`crc32`]'s remainder of each byte, so it takes one step a byte rather than
/// eight.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut crc = byte as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
            bit += 1;
        }
        table[byte] = crc;
        byte += 1;
    }
    table
};

/// Zips built for tests, as wheel tooling writes them.
#[cfg(test)]
pub(crate) mod testing {
    pub(crate) use super::CENTRAL_HEADER;
    use super::{END_RECORD, LOCAL_HEADER, crc32};

    /// One member of a zip: deflated, a regular file of mode 644, unless said
    /// otherwise. `flags` bit 3 writes a data descriptor after the data and
    /// leaves the local header's sizes and CRC zero, as a streaming writer does.
    pub(crate) struct M {
        pub(crate) name: String,
        pub(crate) data: Vec<u8>,
        pub(crate) mode: u32,
        pub(crate) method: u16,
        pub(crate) flags: u16,
    }

    impl Default for M {
        fn default() -> Self {
            Self {
                name: String::new(),
                data: Vec::new(),
                mode: 0o100644,
                method: 8,
                flags: 0,
            }
        }
    }

    pub(crate) fn m(name: &str, data: impl AsRef<[u8]>) -> M {
        M {
            name: name.to_string(),
            data: data.as_ref().to_vec(),
            ..M::default()
        }
    }

    /// A zip of `members`: local headers, a central directory, and the end
    /// record, with no extra fields and no comments.
    pub(crate) fn zip(members: &[M]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for member in members {
            let stored = match member.method {
                8 => miniz_oxide::deflate::compress_to_vec(&member.data, 6),
                _ => member.data.clone(),
            };
            let crc = crc32(&member.data);
            let descriptor = member.flags & 8 != 0;
            let local = |value: u32| if descriptor { 0 } else { value };
            let offset = out.len() as u32;
            out.extend_from_slice(&LOCAL_HEADER);
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&member.flags.to_le_bytes());
            out.extend_from_slice(&member.method.to_le_bytes());
            out.extend_from_slice(&[0; 4]); // time, date
            out.extend_from_slice(&local(crc).to_le_bytes());
            out.extend_from_slice(&local(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&local(member.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(member.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(member.name.as_bytes());
            out.extend_from_slice(&stored);
            if descriptor {
                out.extend_from_slice(&[0x50, 0x4b, 0x07, 0x08]);
                out.extend_from_slice(&crc.to_le_bytes());
                out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
                out.extend_from_slice(&(member.data.len() as u32).to_le_bytes());
            }

            central.extend_from_slice(&CENTRAL_HEADER);
            central.extend_from_slice(&(3u16 << 8 | 20).to_le_bytes()); // made on Unix
            central.extend_from_slice(&20u16.to_le_bytes()); // version needed
            central.extend_from_slice(&member.flags.to_le_bytes());
            central.extend_from_slice(&member.method.to_le_bytes());
            central.extend_from_slice(&[0; 4]); // time, date
            central.extend_from_slice(&crc.to_le_bytes());
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
}

#[cfg(test)]
mod tests {
    use super::testing::{M, m, zip};
    use super::*;

    /// Every member under `prefix/`, as `build.rs` places a pinned wheel's.
    fn under(zip: &[u8], prefix: &str) -> Result<Vec<u8>, String> {
        zip_to_tar(zip, |name, _| Ok(Some(format!("{prefix}/{name}"))))
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
    fn the_crc_is_zips() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn stored_and_deflated_members_land_where_placed_with_their_modes() {
        let wheel = zip(&[
            m("rpyc/__init__.py", b"from rpyc.core import Connection\n"),
            M {
                method: 0,
                ..m("rpyc-6.0.2.dist-info/WHEEL", b"Wheel-Version: 1.0\n")
            },
            M {
                mode: 0o100755,
                ..m("rpyc-6.0.2.data/scripts/tool", b"#!/bin/sh\n")
            },
            M {
                mode: 0,
                ..m("rpyc-6.0.2.dist-info/RECORD", b"")
            },
        ]);
        let tar = under(&wheel, "rpyc-6.0.2-py3-none-any").unwrap();
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
                (
                    "rpyc-6.0.2-py3-none-any/rpyc-6.0.2.dist-info/RECORD".to_string(),
                    0o644,
                    Vec::new()
                ),
            ]
        );
    }

    /// What a writer streaming to a pipe, or one that lists directories, emits
    /// reads as the files alone.
    #[test]
    fn directory_entries_and_data_descriptors_read_as_the_files() {
        let plain = zip(&[m("pkg/__init__.py", b"ANSWER = 42\n")]);
        let written = zip(&[
            M {
                method: 0,
                mode: 0o40755,
                ..m("pkg/", b"")
            },
            M {
                flags: 8,
                ..m("pkg/__init__.py", b"ANSWER = 42\n")
            },
        ]);
        assert_eq!(
            members(&under(&written, "p").unwrap()),
            members(&under(&plain, "p").unwrap())
        );
    }

    #[test]
    fn bytes_that_are_not_a_zip_are_refused() {
        let err = under(b"not a zip at all, and longer than a record", "p").unwrap_err();
        assert!(err.contains("no end-of-central-directory record"), "{err}");
        let err = under(b"short", "p").unwrap_err();
        assert!(err.contains("no end-of-central-directory record"), "{err}");
    }

    #[test]
    fn a_member_that_would_land_outside_its_tree_is_refused() {
        let names = [
            "../escape",
            "/etc/passwd",
            "a/../b",
            "a/./b",
            "/",
            "",
            "back\\slash",
            "a//b",
        ];
        for name in names {
            let err = under(&zip(&[m(name, b"x")]), "p").unwrap_err();
            assert!(
                err.contains("not a plain relative file path"),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn a_member_the_reader_does_not_handle_is_refused() {
        let err = under(
            &zip(&[M {
                method: 12,
                ..m("rpyc/x.py", b"x")
            }]),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("compression method 12"), "{err}");
        let err = under(
            &zip(&[M {
                flags: 1,
                ..m("rpyc/x.py", b"x")
            }]),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("rpyc/x.py is encrypted"), "{err}");
        // Stored bytes that are not a deflate stream.
        let mut wheel = zip(&[m("rpyc/x.py", b"some source text here")]);
        wheel[30 + "rpyc/x.py".len()] ^= 0xff;
        let err = under(&wheel, "p").unwrap_err();
        assert!(err.contains("inflating rpyc/x.py"), "{err}");
    }

    #[test]
    fn a_link_or_other_special_file_is_refused() {
        let err = under(
            &zip(&[M {
                mode: 0o120777,
                ..m("pkg/linked.py", b"agents.py")
            }]),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("pkg/linked.py is a symbolic link"), "{err}");
        let err = under(
            &zip(&[M {
                mode: 0o10644,
                ..m("pkg/fifo", b"")
            }]),
            "p",
        )
        .unwrap_err();
        assert!(err.contains("pkg/fifo is not a regular file"), "{err}");
    }

    #[test]
    fn a_member_whose_bytes_are_not_the_ones_declared_is_refused() {
        let mut wheel = zip(&[M {
            method: 0,
            ..m("pkg/x.py", b"ANSWER = 42\n")
        }]);
        wheel[30 + "pkg/x.py".len()] ^= 0x01;
        let err = under(&wheel, "p").unwrap_err();
        assert!(err.contains("pkg/x.py does not match its CRC-32"), "{err}");
    }

    /// A member `place` leaves out is never inflated: here one that would not
    /// inflate, beside one that does.
    #[test]
    fn a_member_left_out_is_never_read() {
        let mut wheel = zip(&[
            m(".git/HEAD", b"ref: refs/heads/trunk\n"),
            m("pkg/x.py", b"ANSWER = 42\n"),
        ]);
        wheel[30 + ".git/HEAD".len()] ^= 0xff;
        let err = under(&wheel, "p").unwrap_err();
        assert!(err.contains(".git/HEAD"), "{err}");
        let mut asked = Vec::new();
        let tar = zip_to_tar(&wheel, |name, size| {
            asked.push((name.to_string(), size));
            Ok((!name.starts_with(".git/")).then(|| name.to_string()))
        })
        .unwrap();
        assert_eq!(
            asked,
            [(".git/HEAD".to_string(), 22), ("pkg/x.py".to_string(), 12)]
        );
        assert_eq!(
            members(&tar),
            [("pkg/x.py".to_string(), 0o644, b"ANSWER = 42\n".to_vec())]
        );
        let err = zip_to_tar(&wheel, |_, _| Err("refused".to_string())).unwrap_err();
        assert_eq!(err, "refused");
    }
}
