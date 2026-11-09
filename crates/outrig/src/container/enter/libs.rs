// The payload's `DT_NEEDED` closure, resolved against the sidecar's rootfs.
//
// A dynamic loader finds its libraries by name, and after the launcher's setns
// every absolute name it builds for itself means the *primary's* file (see
// `canon.rs`): an absolute symlink under `--library-path`, the loader's
// built-in directories, the entries of musl's `/etc/ld-musl-<arch>.path`. No
// loader option turns those off, so instead of hoping the loader never reaches
// them this walk settles, before the setns, which file each library name
// stands for. The launcher then stages one graft-prefixed link per name in a
// directory that leads `--library-path`, so the loader finds every library the
// payload needs there and nowhere else -- and a name this walk cannot settle
// fails the launch, rather than being found in the primary (task 0003-01).
//
// What it does not cover: a library the payload `dlopen`s at run time (not in
// any `DT_NEEDED`), a `DT_NEEDED` name containing a `/` (the loader opens it as
// written), and an absolute `DT_RPATH` entry, which the loader searches before
// `--library-path`.
//
// Does I/O, through `canon.rs`'s resolution and by reading the objects. Like
// the other files here `launcher.rs` pulls it in with `include!`, so its header
// is plain `//` comments and it declares no `use`; the `outrig` crate also
// compiles it as `#[cfg(test)] mod libs` for the host tests.

/// Why the closure could not be settled. Names and paths are bytes, as the
/// objects hold them; they are only rendered lossily to be read.
#[derive(Debug, PartialEq, Eq)]
enum ClosureError {
    /// No directory the loader would search under the sidecar's rootfs holds
    /// `name`, which `needed_by` asks for.
    Missing {
        name: Vec<u8>,
        needed_by: Vec<u8>,
        searched: Vec<Vec<u8>>,
    },
    /// `path` is in the closure but could not be read or parsed.
    Object { path: Vec<u8>, why: String },
}

fn lossy(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { name, needed_by, searched } => write!(
                f,
                "{} (needed by {}) is not in the sidecar image; searched {}",
                lossy(name),
                lossy(needed_by),
                searched.iter().map(|d| lossy(d)).collect::<Vec<_>>().join(":"),
            ),
            Self::Object { path, why } => write!(f, "read {}: {why}", lossy(path)),
        }
    }
}

/// `root` + `path`, as the bytes `canonicalize_under_root`'s own probes use.
fn under_root(root: &[u8], path: &[u8]) -> std::path::PathBuf {
    let mut full = root.to_vec();
    full.extend_from_slice(path);
    as_path(&full).to_path_buf()
}

/// `bytes` with every `from` replaced by `to`.
fn replace_bytes(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

/// The directories musl's `/etc/ld-musl-<arch>.path` names, for `loader` (its
/// canonical, root-relative path). musl reads the file relative to its own
/// pathname -- the directory holding it, minus a trailing `/lib` -- and
/// splits on `:` and newlines. No such loader name, or no file, is no
/// directories.
fn musl_path_dirs(root: &[u8], loader: &str) -> Vec<Vec<u8>> {
    let Some((dir, base)) = loader.rsplit_once('/') else {
        return Vec::new();
    };
    let Some(arch) = base
        .strip_prefix("ld-musl-")
        .and_then(|b| b.strip_suffix(".so.1"))
    else {
        return Vec::new();
    };
    let prefix = dir.strip_suffix("/lib").unwrap_or(dir);
    let file = format!("{prefix}/etc/ld-musl-{arch}.path");
    let Ok(file) = canonicalize_under_root(root, file.as_bytes()) else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read(under_root(root, &file)) else {
        return Vec::new();
    };
    text.split(|&b| b == b':' || b == b'\n')
        .filter(|d| d.starts_with(b"/"))
        .map(<[u8]>::to_vec)
        .collect()
}

/// Names the loader supplies itself, so nothing needs to be found for them.
fn loader_provides(name: &[u8], loader: &str) -> bool {
    name.contains(&b'/')
        || loader.rsplit('/').next().map(str::as_bytes) == Some(name)
        || name.starts_with(b"libc.musl-")
        || name.starts_with(b"linux-vdso")
        || name.starts_with(b"linux-gate")
}

/// `(name, canonical path)` for each library the closure settled, one per file
/// name the loader will be asked for -- a name a loaded library's `DT_SONAME`
/// already answers has no entry.
type Closure = Vec<(Vec<u8>, Vec<u8>)>;

/// One object of the closure, with what it was reached through.
struct Pending {
    path: Vec<u8>,
    needed: Needed,
    /// The `DT_RPATH` directories of the objects that led here, nearest first.
    /// glibc offers them to this object's dependencies when this object has no
    /// `DT_RUNPATH` of its own.
    inherited: Vec<Vec<u8>>,
}

/// Settle every library `program` needs, transitively, under `root`: each
/// `(name, canonical path)` is the first hit in the order glibc searches --
/// `DT_RPATH` (the requesting object's, then its ancestors'), then `lib_dirs`
/// (the `--library-path`), then the requester's `DT_RUNPATH`, then `musl_dirs`
/// -- or musl's order, which inherits `DT_RUNPATH` as well -- with every symlink on the way resolved inside `root`. A candidate that is
/// not an ELF64 of the program's machine is skipped, as the loader skips it;
/// a corrupt one is an error.
///
/// `musl` is whether the payload's `PT_INTERP` names musl's loader -- not
/// read from `loader`, which is canonical and so may carry another name.
///
/// Must run before the setns, for the reason `canonicalize_under_root` gives.
fn resolve_closure(
    root: &[u8],
    program: &str,
    loader: &str,
    musl: bool,
    lib_dirs: &[String],
    musl_dirs: &[Vec<u8>],
) -> Result<Closure, ClosureError> {
    let object_err = |path: &[u8], why: String| ClosureError::Object { path: path.to_vec(), why };
    let program = program.as_bytes().to_vec();
    let bytes = std::fs::read(under_root(root, &program))
        .map_err(|e| object_err(&program, e.to_string()))?;
    let needed = elf_needed(&bytes).map_err(|e| object_err(&program, e.to_string()))?;
    let machine = elf64_machine(&bytes).ok_or_else(|| object_err(&program, ElfError::NotElf64.to_string()))?;
    drop(bytes);

    let lib_dirs: Vec<Vec<u8>> = lib_dirs.iter().map(|d| d.as_bytes().to_vec()).collect();
    let mut found: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    // `DT_SONAME`s of the libraries selected so far: the loader reuses a loaded
    // object for any name it carries, whether or not a file has that name.
    let mut aliases: Vec<Vec<u8>> = Vec::new();
    // First in, first out, as the loader maps dependencies: all of an object's
    // own before any of theirs, so the first object to name a library is the
    // one whose search finds it, and the name is settled then.
    let mut queue = std::collections::VecDeque::from([Pending {
        path: program,
        needed,
        inherited: Vec::new(),
    }]);
    while let Some(Pending { path: object, needed, inherited }) = queue.pop_front() {
        let origin = match object.iter().rposition(|&b| b == b'/') {
            Some(0) | None => &b"/"[..],
            Some(i) => &object[..i],
        };
        let expand = |values: &[Vec<u8>]| -> Vec<Vec<u8>> {
            values
                .iter()
                .flat_map(|v| v.split(|&b| b == b':'))
                .filter_map(|raw| {
                    let raw = replace_bytes(raw, b"${ORIGIN}", origin);
                    let raw = replace_bytes(&raw, b"$ORIGIN", origin);
                    canonicalize_under_root(root, &raw).ok()
                })
                .collect()
        };
        let (own_rpath, own_runpath) = (expand(&needed.rpath), expand(&needed.runpath));
        // Tag presence decides, not what the tags expand to: an empty
        // `DT_RUNPATH` still counts as one.
        let has_runpath = !needed.runpath.is_empty();
        let mut dirs: Vec<Vec<u8>> = Vec::new();
        let child_inherited: Vec<Vec<u8>> = if musl {
            // musl hands an ancestor's `DT_RUNPATH` (or, lacking one,
            // `DT_RPATH`) down its `needed_by` chain; its own comes after the
            // `--library-path`, ahead of the path file.
            let own = if has_runpath { own_runpath } else { own_rpath };
            dirs.extend(lib_dirs.iter().cloned());
            dirs.extend(own.iter().cloned());
            dirs.extend(inherited.iter().cloned());
            dirs.extend(musl_dirs.iter().cloned());
            own.into_iter().chain(inherited).collect()
        } else {
            // glibc searches the `DT_RPATH` of the object and then its
            // ancestors -- unless the object has a `DT_RUNPATH`, which turns
            // all of that off, and which only its own dependencies see, after
            // the `--library-path`.
            if !has_runpath {
                dirs.extend(own_rpath.iter().cloned());
                dirs.extend(inherited.iter().cloned());
            }
            dirs.extend(lib_dirs.iter().cloned());
            dirs.extend(own_runpath);
            if has_runpath {
                inherited
            } else {
                own_rpath.into_iter().chain(inherited).collect()
            }
        };

        for name in needed.names {
            if loader_provides(&name, loader)
                || found.iter().any(|(n, _)| *n == name)
                || aliases.contains(&name)
            {
                continue;
            }
            let mut hit = None;
            for dir in &dirs {
                let mut candidate = dir.clone();
                candidate.push(b'/');
                candidate.extend_from_slice(&name);
                let Ok(canon) = canonicalize_under_root(root, &candidate) else {
                    continue;
                };
                let full = under_root(root, &canon);
                if !std::fs::metadata(&full).is_ok_and(|m| m.is_file()) {
                    continue;
                }
                // An unreadable or foreign candidate is one the loader steps
                // over, so it is not this walk's to fail on; only a malformed
                // object of the program's own machine is.
                let Ok(bytes) = std::fs::read(&full) else {
                    continue;
                };
                if elf64_machine(&bytes) != Some(machine) {
                    continue;
                }
                match elf_needed(&bytes) {
                    Ok(n) => {
                        hit = Some((canon, n));
                        break;
                    }
                    Err(e) => return Err(object_err(&canon, e.to_string())),
                }
            }
            let Some((canon, n)) = hit else {
                return Err(ClosureError::Missing {
                    name,
                    needed_by: object,
                    searched: dirs,
                });
            };
            aliases.extend(n.soname.iter().cloned());
            found.push((name, canon.clone()));
            queue.push_back(Pending { path: canon, needed: n, inherited: child_inherited.clone() });
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rootfs under a tempdir, as `canon.rs`'s tests stage one.
    struct Root(tempfile::TempDir);

    impl Root {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("tempdir"))
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.path().as_os_str().as_encoded_bytes().to_vec()
        }

        fn parent_of(&self, path: &str) -> std::path::PathBuf {
            let p = self.0.path().join(path.trim_start_matches('/'));
            std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir -p");
            p
        }

        fn file(&self, path: &str, content: &[u8]) {
            std::fs::write(self.parent_of(path), content).expect("write");
        }

        fn lib(&self, path: &str, needed: &[&str], runpath: Option<&str>) {
            self.file(path, &elf64_dynamic(needed, None, runpath));
        }

        fn lib_rpath(&self, path: &str, needed: &[&str], rpath: &str) {
            self.file(path, &elf64_dynamic(needed, Some(rpath), None));
        }

        fn link(&self, path: &str, target: &str) {
            std::os::unix::fs::symlink(target, self.parent_of(path)).expect("symlink");
        }
    }

    fn dirs(d: &[&str]) -> Vec<String> {
        d.iter().map(|s| s.to_string()).collect()
    }

    const LOADER: &str = "/lib/ld-musl-x86_64.so.1";
    const GLIBC: &str = "/lib64/ld-linux-x86-64.so.2";

    #[test]
    fn absolute_symlink_resolves_inside_the_root() {
        let r = Root::new();
        r.lib("/usr/bin/xz", &["liblzma.so.5", "libc.musl-x86_64.so.1"], None);
        r.file("/opt/real/liblzma.so.5.8.4", &elf64_dynamic(&[], None, None));
        r.link("/usr/lib/liblzma.so.5", "/opt/real/liblzma.so.5.8.4");
        let got =
            resolve_closure(&r.bytes(), "/usr/bin/xz", LOADER, true, &dirs(&["/usr/lib"]), &[]).unwrap();
        assert_eq!(got, [(b"liblzma.so.5".to_vec(), b"/opt/real/liblzma.so.5.8.4".to_vec())]);
    }

    #[test]
    fn missing_library_is_named() {
        let r = Root::new();
        r.lib("/usr/bin/xz", &["liblzma.so.5"], None);
        let err =
            resolve_closure(&r.bytes(), "/usr/bin/xz", LOADER, true, &dirs(&["/usr/lib"]), &[]).unwrap_err();
        assert_eq!(
            err,
            ClosureError::Missing {
                name: b"liblzma.so.5".to_vec(),
                needed_by: b"/usr/bin/xz".to_vec(),
                searched: vec![b"/usr/lib".to_vec()],
            }
        );
    }

    #[test]
    fn musl_path_file_names_the_directory() {
        let r = Root::new();
        r.lib("/usr/bin/xz", &["liblzma.so.5"], None);
        r.file("/opt/hidden/liblzma.so.5", &elf64_dynamic(&[], None, None));
        r.file("/etc/ld-musl-x86_64.path", b"/opt/hidden:/lib\n");
        let musl = musl_path_dirs(&r.bytes(), LOADER);
        assert_eq!(musl, [b"/opt/hidden".to_vec(), b"/lib".to_vec()]);
        let got = resolve_closure(&r.bytes(), "/usr/bin/xz", LOADER, true, &dirs(&["/usr/lib"]), &musl)
            .unwrap();
        assert_eq!(got, [(b"liblzma.so.5".to_vec(), b"/opt/hidden/liblzma.so.5".to_vec())]);
    }

    #[test]
    fn no_path_file_is_no_directories() {
        let r = Root::new();
        assert!(musl_path_dirs(&r.bytes(), LOADER).is_empty());
        assert!(musl_path_dirs(&r.bytes(), "/lib64/ld-linux-x86-64.so.2").is_empty());
    }

    #[test]
    fn dependencies_are_followed_and_loader_names_skipped() {
        let r = Root::new();
        r.lib("/usr/bin/app", &["libfoo.so.1", "ld-musl-x86_64.so.1"], None);
        r.lib("/usr/lib/libfoo.so.1", &["libbar.so.2", "libc.so.6"], None);
        r.lib("/usr/lib/libbar.so.2", &[], None);
        r.lib("/usr/lib/libc.so.6", &[], None);
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &dirs(&["/usr/lib"]), &[])
            .unwrap();
        let mut names: Vec<_> = got.iter().map(|(n, _)| n.as_slice()).collect();
        names.sort();
        assert_eq!(names, [&b"libbar.so.2"[..], b"libc.so.6", b"libfoo.so.1"]);
    }

    #[test]
    fn runpath_origin_is_expanded() {
        let r = Root::new();
        r.lib("/opt/app/bin/app", &["libpriv.so"], Some("$ORIGIN/../private"));
        r.lib("/opt/app/private/libpriv.so", &[], None);
        let got = resolve_closure(&r.bytes(), "/opt/app/bin/app", LOADER, true, &[], &[]).unwrap();
        assert_eq!(got, [(b"libpriv.so".to_vec(), b"/opt/app/private/libpriv.so".to_vec())]);
    }

    #[test]
    fn rpath_is_inherited_by_dependencies_of_dependencies() {
        let r = Root::new();
        r.lib_rpath("/opt/app/bin/app", &["libparent.so"], "$ORIGIN/../private");
        r.lib("/opt/app/private/libparent.so", &["libchild.so"], None);
        r.lib("/opt/app/private/libchild.so", &[], None);
        let got = resolve_closure(&r.bytes(), "/opt/app/bin/app", GLIBC, false, &[], &[]).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn runpath_is_not_inherited() {
        let r = Root::new();
        r.lib("/opt/app/bin/app", &["libparent.so"], Some("$ORIGIN/../private"));
        r.lib("/opt/app/private/libparent.so", &["libchild.so"], None);
        r.lib("/opt/app/private/libchild.so", &[], None);
        let err = resolve_closure(&r.bytes(), "/opt/app/bin/app", GLIBC, false, &[], &[]).unwrap_err();
        assert!(matches!(err, ClosureError::Missing { ref name, .. } if name == b"libchild.so"));
    }

    #[test]
    fn a_wrong_class_candidate_is_skipped() {
        let r = Root::new();
        r.lib("/usr/bin/app", &["libfoo.so"], None);
        let mut elf32 = elf64_dynamic(&[], None, None);
        elf32[4] = 1; // ELFCLASS32
        r.file("/usr/local/lib/libfoo.so", &elf32);
        r.lib("/usr/lib/libfoo.so", &[], None);
        let dirs = dirs(&["/usr/local/lib", "/usr/lib"]);
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &dirs, &[]).unwrap();
        assert_eq!(got, [(b"libfoo.so".to_vec(), b"/usr/lib/libfoo.so".to_vec())]);
    }

    #[test]
    fn a_wrong_machine_candidate_is_skipped_but_a_corrupt_one_is_not() {
        let r = Root::new();
        r.lib("/usr/bin/app", &["libfoo.so"], None);
        let mut other = elf64_dynamic(&[], None, None);
        other[18] = 0xb7; // EM_AARCH64 against the program's 0
        r.file("/usr/local/lib/libfoo.so", &other);
        r.lib("/usr/lib/libfoo.so", &[], None);
        let both = dirs(&["/usr/local/lib", "/usr/lib"]);
        assert!(resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &both, &[]).is_ok());

        r.file("/usr/local/lib/libfoo.so", &elf64_dynamic(&["x"], None, None)[..70]);
        let err = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &both, &[]).unwrap_err();
        assert!(matches!(err, ClosureError::Object { .. }));
    }

    #[test]
    fn non_utf8_names_are_kept_as_bytes() {
        let r = Root::new();
        // `elf64_dynamic` takes `&str`, so patch a placeholder name's bytes.
        let mut app = elf64_dynamic(&["libXfoo.so"], None, None);
        let at = app.windows(10).position(|w| w == b"libXfoo.so").unwrap();
        app[at + 3] = 0xff;
        r.file("/usr/bin/app", &app);
        let name = {
            let mut n = b"libXfoo.so".to_vec();
            n[3] = 0xff;
            n
        };
        std::fs::write(
            r.parent_of("/usr/lib/x").with_file_name(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&name)),
            elf64_dynamic(&[], None, None),
        )
        .expect("write");
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &dirs(&["/usr/lib"]), &[])
            .unwrap();
        assert_eq!(got[0].0, name);
    }

    #[test]
    fn musl_inherits_runpath() {
        let r = Root::new();
        r.lib("/opt/app/bin/app", &["libparent.so"], Some("$ORIGIN/../private"));
        r.lib("/opt/app/private/libparent.so", &["libchild.so"], None);
        r.lib("/opt/app/private/libchild.so", &[], None);
        let got = resolve_closure(&r.bytes(), "/opt/app/bin/app", LOADER, true, &[], &[]).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn an_empty_runpath_still_switches_inherited_rpath_off() {
        let r = Root::new();
        r.lib_rpath("/opt/app/bin/app", &["libparent.so"], "/opt/app/private");
        r.lib("/opt/app/private/libparent.so", &["libchild.so"], Some(""));
        r.lib("/opt/app/private/libchild.so", &["libincompatible.so"], None);
        r.lib("/usr/lib/libchild.so", &[], None);
        let dirs = dirs(&["/usr/lib"]);
        let got = resolve_closure(&r.bytes(), "/opt/app/bin/app", GLIBC, false, &dirs, &[]).unwrap();
        let child = got.iter().find(|(n, _)| n == b"libchild.so").unwrap();
        assert_eq!(child.1, b"/usr/lib/libchild.so");
    }

    #[test]
    fn an_unreadable_candidate_is_stepped_over() {
        use std::os::unix::fs::PermissionsExt;
        let r = Root::new();
        r.lib("/usr/bin/app", &["libfoo.so"], None);
        r.lib("/usr/local/lib/libfoo.so", &[], None);
        let early = r.0.path().join("usr/local/lib/libfoo.so");
        std::fs::set_permissions(&early, std::fs::Permissions::from_mode(0o000)).unwrap();
        r.lib("/usr/lib/libfoo.so", &[], None);
        // Root reads through mode 0, so only assert where the mode bites.
        let bites = std::fs::read(&early).is_err();
        let both = dirs(&["/usr/local/lib", "/usr/lib"]);
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &both, &[]).unwrap();
        if bites {
            assert_eq!(got[0].1, b"/usr/lib/libfoo.so");
        }
    }

    #[test]
    fn a_foreign_candidate_is_filtered_before_its_headers_are_parsed() {
        let r = Root::new();
        r.lib("/usr/bin/app", &["libfoo.so"], None);
        let mut foreign = elf64_dynamic(&[], None, None);
        foreign[18] = 0xb7;
        foreign.truncate(64 + 8); // program headers cut off mid-entry
        r.file("/usr/local/lib/libfoo.so", &foreign);
        r.lib("/usr/lib/libfoo.so", &[], None);
        let both = dirs(&["/usr/local/lib", "/usr/lib"]);
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", LOADER, true, &both, &[]).unwrap();
        assert_eq!(got[0].1, b"/usr/lib/libfoo.so");
    }

    #[test]
    fn a_symlinked_musl_loader_still_gets_musl_rules() {
        let r = Root::new();
        r.lib("/opt/app/bin/app", &["libparent.so"], Some("$ORIGIN/../private"));
        r.lib("/opt/app/private/libparent.so", &["libchild.so"], None);
        r.lib("/opt/app/private/libchild.so", &[], None);
        // The canonical loader path carries no `ld-musl` in its name.
        let got = resolve_closure(
            &r.bytes(),
            "/opt/app/bin/app",
            "/lib/x86_64-linux-musl/libc.so",
            true,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn the_first_object_to_name_a_library_settles_it() {
        let r = Root::new();
        r.lib("/opt/app/bin/app", &["liba.so", "libb.so"], None);
        r.lib("/opt/app/a/liba.so", &["libc2.so"], Some("/opt/app/a"));
        r.lib("/opt/app/b/libb.so", &["libc2.so"], Some("/opt/app/b"));
        r.lib("/opt/app/a/libc2.so", &[], None);
        r.lib("/opt/app/b/libc2.so", &[], None);
        let dirs = dirs(&["/opt/app/a", "/opt/app/b"]);
        let got = resolve_closure(&r.bytes(), "/opt/app/bin/app", GLIBC, false, &dirs, &[]).unwrap();
        let c = got.iter().find(|(n, _)| n == b"libc2.so").unwrap();
        assert_eq!(c.1, b"/opt/app/a/libc2.so");
    }

    #[test]
    fn a_soname_answers_a_later_name_without_a_file() {
        let r = Root::new();
        r.lib("/usr/bin/app", &["liba.so", "libb.so"], None);
        r.file(
            "/usr/lib/liba.so",
            &elf64_dynamic_soname(&[], None, None, Some("libb.so")),
        );
        let got = resolve_closure(&r.bytes(), "/usr/bin/app", GLIBC, false, &dirs(&["/usr/lib"]), &[])
            .unwrap();
        assert_eq!(got, [(b"liba.so".to_vec(), b"/usr/lib/liba.so".to_vec())]);
    }
}
