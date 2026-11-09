//! Parsing and formatting for the container's `/etc/passwd` and `/etc/group`.
//!
//! Everything here is pure: no podman, no namespaces, no I/O. It is the half
//! of [`Container::bootstrap_user`] that used to be delegated to `getent`,
//! `groupadd`, and `useradd` running inside the image.
//!
//! [`Container::bootstrap_user`]: super::Container::bootstrap_user

use std::borrow::Cow;
use std::path::Path;

use crate::config::fold_container_path;

/// Login shell written into new `/etc/passwd` entries.
///
/// `useradd`'s own default is distro-dependent -- Debian's
/// `/etc/default/useradd` sets `/bin/sh`, while upstream shadow compiles in
/// `/bin/bash`, which Alpine doesn't ship -- so the entries it produced were
/// never identical across images either. `/bin/sh` exists everywhere.
const DEFAULT_SHELL: &str = "/bin/sh";

/// Name of the first entry whose numeric id equals `id` -- the third field in
/// both databases. This is what `getent passwd <n>` / `getent group <n>`
/// resolve to: glibc parses an all-numeric key as an id, and `nss_files`
/// answers with the first matching line of the file.
pub(super) fn lookup_id(text: &str, id: u32) -> Option<String> {
    entries(text.as_bytes()).find_map(|entry| {
        let (name, found) = entry_id(&entry.text)?;
        (found == id).then(|| name.to_string())
    })
}

/// Whether `name` already occupies the first field of some entry.
pub(super) fn name_taken(text: &str, name: &str) -> bool {
    entries(text.as_bytes()).any(|entry| entry.text.split(':').next() == Some(name))
}

/// `candidate`, else `candidate_`, `candidate__`, ... up to `retries`
/// attempts total; `None` when every one of them is taken. This is the
/// `_`-suffix loop the `groupadd`/`useradd` path drove through exit codes,
/// with the collision now visible directly in the file.
pub(super) fn free_name(text: &str, candidate: &str, retries: usize) -> Option<String> {
    let mut name = candidate.to_string();
    for _ in 0..retries {
        if !name_taken(text, &name) {
            return Some(name);
        }
        name.push('_');
    }
    None
}

/// Reduce a host user or group name to something that can't corrupt a
/// colon-delimited database. `useradd` used to reject these for us (a name
/// with a `:` in it failed, we appended `_`, and eventually gave up); writing
/// the file ourselves means rejecting them ourselves.
pub(super) fn sanitize_name(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('-');
    if is_path_component(cleaned) {
        cleaned.to_string()
    } else {
        fallback.to_string()
    }
}

/// Whether `name` can stand as the last component of [`home_dir`]: anything
/// else walks out of `/home` (`..`, `../etc`), stays on it (`.`), or makes
/// directories on the way (`a/b`). A name reused from the image's
/// `/etc/passwd` is taken verbatim, so it is held to this the same as a
/// sanitized host name is.
pub(super) fn is_path_component(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains(['/', '\0'])
}

/// The in-container home directory for the user at `uid`, named `name`, given
/// `passwd` -- that file as it is, or as the user is about to be appended to
/// it -- and `workspace`, where the checkout is bind-mounted in the container
/// (empty when there is none). The one definition: it is written into
/// `/etc/passwd` -- by [`passwd_line`] or, for an entry reused from the image or
/// planted by podman, rewritten there by [`rehome`] -- created and `chown`ed
/// after that, and passed as `HOME` to every `podman exec`. All three have to
/// agree.
///
/// `/home/<name>`, except at uid 0, where the image's home for root is kept:
/// it is where the image's build left root's dotfiles and toolchains, and
/// [`rehome`] then has nothing to do. That is the home the entry names, when it
/// [is usable](is_usable_home) and is not the `workspace` or inside it -- a home
/// in the checkout would put root's caches in the user's repository, and
/// `create_home` would `chown` the user's own directory. Otherwise root's home
/// is `/root`, which is what an image with no entry at uid 0 gets, or
/// `/home/<name>` when `/root` is in the `workspace` too. The `workspace` is
/// compared [folded](fold_container_path), as podman folds a destination before
/// it mounts there: `/var/../root` is `/root`. A home that is not UTF-8 is not
/// usable either: decoded lossily it would be made and exported as a directory
/// the entry does not name.
///
/// The entry has to be the image's, and the bytes cannot say whether it is. For
/// an image with no account at the uid its `--user` names -- which
/// `--userns=keep-id` implies -- podman plants one whose home is the
/// container's working directory: the checkout for a primary, the image's
/// `WORKDIR` for a container with no workspace, which an extra mount can cover.
/// That is no home for root, so a caller that takes the entry for podman's
/// passes an empty `passwd`, as for an image with none.
pub(super) fn home_dir(passwd: &[u8], uid: u32, name: &str, workspace: &Path) -> String {
    let named = format!("/home/{name}");
    if uid != 0 {
        return named;
    }
    let home = entry_at(passwd, uid)
        .and_then(|entry| entry.line.split(|&b| b == b':').nth(5))
        .and_then(|home| std::str::from_utf8(home).ok())
        .filter(|home| is_usable_home(home) && !in_workspace(home, workspace))
        .unwrap_or("/root");
    // The entry's home was kept out of the workspace above; only `/root` can be in it.
    if in_workspace(home, workspace) {
        named
    } else {
        home.to_string()
    }
}

/// Whether `home` is `workspace` or inside it, by path component: `/workspace2`
/// is not inside `/workspace`, and a `workspace` of `/var/../root` is `/root`.
/// An empty `workspace` -- the container has none -- holds nothing, though
/// [`Path::starts_with`] takes it for the parent of every path and
/// [`fold_container_path`] for `/`.
fn in_workspace(home: &str, workspace: &Path) -> bool {
    !workspace.as_os_str().is_empty() && Path::new(home).starts_with(fold_container_path(workspace))
}

/// Whether a home that an image's `/etc/passwd` names can go to `mkdir -p` and
/// `fchown` in the container as root just as it is written: a `/` and then
/// [plain names](is_path_component) separated by single `/`s. So not `/`
/// itself, which `create_home` has no directory to make for; not a `..`, which
/// would walk the `chown` out of the home (`/root/..` is the container's `/`);
/// and not a trailing `/`, which makes `O_NOFOLLOW` follow a symlink at the end
/// of the path.
fn is_usable_home(home: &str) -> bool {
    home.strip_prefix('/')
        .is_some_and(|below| below.split('/').all(is_path_component))
}

/// How to make the entry at a uid name a given home: truncate the file at
/// byte `at`, then append `tail`.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rehome {
    pub(super) at: usize,
    pub(super) tail: Vec<u8>,
    /// The home the entry named before, for the log.
    pub(super) was: String,
}

/// The rewrite that makes the `/etc/passwd` entry at `uid` -- the one
/// [`lookup_id`] finds -- name `home`, or `None` when it already does, when
/// there is no entry at `uid`, or when its line is too short to carry a home
/// field at all.
///
/// Only the sixth field changes, and nothing before the entry is rewritten.
/// The shell field may be missing entirely: glibc reads a six-field line's
/// last field as the home, with an empty shell. It works on the raw bytes
/// because [`UserDb::read`](super::namespace::UserDb::read) decodes lossily: a
/// non-UTF-8 GECOS written back from that would come back as `U+FFFD`.
pub(super) fn rehome(raw: &[u8], uid: u32, home: &str) -> Option<Rehome> {
    let entry = entry_at(raw, uid)?;
    let mut fields: Vec<&[u8]> = entry.line.splitn(7, |&b| b == b':').collect();
    let was = *fields.get(5)?;
    if was == home.as_bytes() {
        return None;
    }
    fields[5] = home.as_bytes();
    let mut tail = fields.join(&b':');
    tail.extend_from_slice(&raw[entry.at + entry.line.len()..]);
    Some(Rehome {
        at: entry.at,
        tail,
        was: String::from_utf8_lossy(was).into_owned(),
    })
}

/// An `/etc/group` line as `groupadd --gid <gid> <name>` writes it: four
/// fields, no members.
pub(super) fn group_line(name: &str, gid: u32) -> String {
    format!("{name}:x:{gid}:\n")
}

/// An `/etc/passwd` line as `useradd -u <uid> -g <gid> <name>` writes it:
/// `x` password, empty GECOS, and a home directory that is named but not
/// created (that is a separate step, exactly as `useradd` without `-m`).
///
/// The home is [`home_dir`] of a file with no entry for this user yet, in a
/// container whose checkout is at `workspace`, so it agrees with the `HOME`
/// that [`Container::build_exec_argv`](super::Container::build_exec_argv)
/// passes to every `podman exec` without the caller having to say so.
pub(super) fn passwd_line(name: &str, uid: u32, gid: u32, workspace: &Path) -> String {
    let home = home_dir(&[], uid, name, workspace);
    format!("{name}:x:{uid}:{gid}::{home}:{DEFAULT_SHELL}\n")
}

/// `line` as bytes to append to a file whose current contents are `existing`,
/// inserting the newline the file is missing rather than gluing the new entry
/// onto the last one.
pub(super) fn append_blob(existing: &str, line: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len() + 1);
    if !existing.is_empty() && !existing.ends_with('\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(line.as_bytes());
    out
}

/// One line of a database that carries an entry.
struct Entry<'a> {
    /// Byte offset the line starts at.
    at: usize,
    /// The line as stored, without its `\n` or `\r\n` -- as `str::lines`
    /// gives it, so a last field is never glued to the terminator.
    line: &'a [u8],
    /// `line`, decoded.
    text: Cow<'a, str>,
}

/// Lines that carry an entry, skipping what `nss_files` skips: blank lines and
/// comments. Malformed lines fall out of the lookups themselves, which need a
/// name and a parseable id. Over bytes, so [`rehome`] can rewrite the very
/// line [`lookup_id`] found without a lossy decode in between.
fn entries(raw: &[u8]) -> impl Iterator<Item = Entry<'_>> {
    raw.split_inclusive(|&b| b == b'\n')
        .scan(0, |at, line| {
            let start = *at;
            *at += line.len();
            Some((start, line))
        })
        .filter_map(|(at, line)| {
            let line = line
                .strip_suffix(b"\n")
                .map_or(line, |l| l.strip_suffix(b"\r").unwrap_or(l));
            let text = String::from_utf8_lossy(line);
            let skipped = text.trim().is_empty() || text.trim_start().starts_with('#');
            (!skipped).then_some(Entry { at, line, text })
        })
}

/// The first entry whose numeric id is `uid`: the line [`lookup_id`] resolves
/// a name from, found over the same iterator.
fn entry_at(raw: &[u8], uid: u32) -> Option<Entry<'_>> {
    entries(raw).find(|entry| entry_id(&entry.text).is_some_and(|(_, id)| id == uid))
}

/// An entry's name and numeric id: its first and third fields.
fn entry_id(line: &str) -> Option<(&str, u32)> {
    let mut fields = line.split(':');
    let name = fields.next()?;
    let id = fields.nth(1)?.parse::<u32>().ok()?;
    Some((name, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALPINE_PASSWD: &str = "root:x:0:0:root:/root:/bin/ash\n\
                                 bin:x:1:1:bin:/bin:/sbin/nologin\n\
                                 nobody:x:65534:65534:nobody:/:/sbin/nologin\n";
    const DEBIAN_GROUP: &str = "root:x:0:\n\
                                daemon:x:1:\n\
                                users:x:100:\n\
                                nogroup:x:65534:\n";

    #[test]
    fn lookup_id_finds_an_existing_entry() {
        assert_eq!(lookup_id(ALPINE_PASSWD, 65534).as_deref(), Some("nobody"));
        assert_eq!(lookup_id(DEBIAN_GROUP, 100).as_deref(), Some("users"));
    }

    #[test]
    fn lookup_id_matches_the_id_field_not_the_name() {
        // A user literally named "1000" must not answer a lookup for uid 1000.
        let text = "1000:x:5:5::/home/odd:/bin/sh\n";
        assert_eq!(lookup_id(text, 1000), None);
        assert_eq!(lookup_id(text, 5).as_deref(), Some("1000"));
    }

    #[test]
    fn lookup_id_skips_comments_blanks_and_malformed_lines() {
        let text = "# a comment\n\
                    \n\
                    broken-line-without-fields\n\
                    tgockel:x:1000:1000::/home/tgockel:/bin/sh\n";
        assert_eq!(lookup_id(text, 1000).as_deref(), Some("tgockel"));
    }

    #[test]
    fn lookup_id_is_none_when_absent() {
        assert_eq!(lookup_id(ALPINE_PASSWD, 1000), None);
    }

    #[test]
    fn free_name_returns_the_candidate_when_unused() {
        assert_eq!(
            free_name(ALPINE_PASSWD, "tgockel", 10).as_deref(),
            Some("tgockel")
        );
    }

    #[test]
    fn free_name_appends_underscores_on_collision() {
        let text = "tgockel:x:5:5::/home/a:/bin/sh\ntgockel_:x:6:6::/home/b:/bin/sh\n";
        assert_eq!(free_name(text, "tgockel", 10).as_deref(), Some("tgockel__"));
    }

    #[test]
    fn free_name_exhausts_after_the_retry_budget() {
        let mut text = String::new();
        let mut name = "tgockel".to_string();
        for i in 0..10 {
            text.push_str(&passwd_line(&name, 100 + i, 100 + i, Path::new("")));
            name.push('_');
        }
        assert_eq!(free_name(&text, "tgockel", 10), None);
        // One more attempt would have found the free name.
        assert_eq!(
            free_name(&text, "tgockel", 11).as_deref(),
            Some("tgockel__________")
        );
    }

    #[test]
    fn sanitize_name_neutralizes_field_and_line_separators() {
        assert_eq!(sanitize_name("ad:user", "u1000"), "ad_user");
        assert_eq!(sanitize_name("two\nlines", "u1000"), "two_lines");
        assert_eq!(sanitize_name("EXAMPLE\\user", "u1000"), "EXAMPLE_user");
        assert_eq!(sanitize_name("ok.name-1_2", "u1000"), "ok.name-1_2");
    }

    #[test]
    fn sanitize_name_falls_back_when_nothing_usable_is_left() {
        assert_eq!(sanitize_name("", "u1000"), "u1000");
        assert_eq!(sanitize_name("---", "u1000"), "u1000");
    }

    #[test]
    fn sanitize_name_falls_back_on_a_name_that_is_not_a_directory() {
        assert_eq!(sanitize_name(".", "u1000"), "u1000");
        assert_eq!(sanitize_name("..", "u1000"), "u1000");
        // Only the two special entries: three dots name an ordinary directory.
        assert_eq!(sanitize_name("...", "u1000"), "...");
    }

    #[test]
    fn is_path_component_accepts_only_a_single_ordinary_component() {
        for ok in ["tgockel", "...", ".hidden", "a.b"] {
            assert!(is_path_component(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", "../etc", "a/b", "/", "nul\0"] {
            assert!(!is_path_component(bad), "{bad:?}");
        }
    }

    #[test]
    fn group_line_matches_groupadd_output() {
        assert_eq!(group_line("tgockel", 1000), "tgockel:x:1000:\n");
    }

    #[test]
    fn passwd_line_matches_useradd_output() {
        assert_eq!(
            passwd_line("tgockel", 1000, 1000, Path::new("")),
            "tgockel:x:1000:1000::/home/tgockel:/bin/sh\n"
        );
    }

    /// An entry bootstrap writes for uid 0 -- an image with no root line --
    /// names `/root`, the home `home_dir` settles on for it afterwards.
    #[test]
    fn passwd_line_puts_root_at_slash_root() {
        assert_eq!(
            passwd_line("root", 0, 0, Path::new("")),
            "root:x:0:0::/root:/bin/sh\n"
        );
    }

    #[test]
    fn append_blob_inserts_the_missing_newline_only_when_needed() {
        let line = group_line("tgockel", 1000);
        assert_eq!(append_blob("root:x:0:\n", &line), line.as_bytes());
        assert_eq!(append_blob("", &line), line.as_bytes());

        let mut expected = vec![b'\n'];
        expected.extend_from_slice(line.as_bytes());
        assert_eq!(append_blob("root:x:0:", &line), expected);
    }

    #[test]
    fn an_appended_entry_is_found_by_a_later_lookup() {
        let line = passwd_line("tgockel", 1000, 1000, Path::new(""));
        let mut text = ALPINE_PASSWD.to_string();
        text.push_str(&String::from_utf8(append_blob(&text, &line)).unwrap());

        assert_eq!(lookup_id(&text, 1000).as_deref(), Some("tgockel"));
        assert_eq!(lookup_id(&text, 0).as_deref(), Some("root"));
        assert!(name_taken(&text, "tgockel"));
    }

    /// An image's root entry naming `home`.
    fn root_entry(home: &str) -> String {
        format!("root:x:0:0:root:{home}:/bin/sh\n")
    }

    /// What bootstrap makes of the entry at uid 0 in `raw`: the home it settles
    /// on, and the file once the entry has been pointed at that home.
    fn settled_root(raw: &[u8]) -> (String, Vec<u8>) {
        settled_root_in(raw, Path::new(""))
    }

    /// [`settled_root`] in a container whose checkout is mounted at `workspace`.
    fn settled_root_in(raw: &[u8], workspace: &Path) -> (String, Vec<u8>) {
        let home = home_dir(raw, 0, "root", workspace);
        let after = rehome(raw, 0, &home).map_or(raw.to_vec(), |rehome| applied(raw, &rehome));
        (home, after)
    }

    /// [`home_dir`] with no workspace to keep out of.
    fn home(passwd: &[u8], uid: u32, name: &str) -> String {
        home_dir(passwd, uid, name, Path::new(""))
    }

    #[test]
    fn home_dir_is_under_home_for_every_uid_but_zero() {
        // podman's entry names the working directory: for `rehome` to move, not
        // to follow.
        let raw = format!("{ALPINE_PASSWD}{}", podman_keep_id_entry("/workspace"));
        assert_eq!(home(raw.as_bytes(), 1000, "tgockel"), "/home/tgockel");
        assert_eq!(home(&[], 1000, "tgockel"), "/home/tgockel");
        // An image's own entry at some other uid is no different.
        assert_eq!(home(ALPINE_PASSWD.as_bytes(), 1, "bin"), "/home/bin");
        // Only uid 0 is special: the name `root` at another uid is not.
        assert_eq!(home(&[], 1000, "root"), "/home/root");
    }

    #[test]
    fn is_usable_home_accepts_only_a_plain_absolute_path_below_the_root() {
        for ok in [
            "/root",
            "/home/root",
            "/var/lib/root",
            "/home/a b",
            "/.hidden",
            "/...",
            "/..x",
        ] {
            assert!(is_usable_home(ok), "{ok:?}");
        }
        for bad in [
            "", "root", "./root", "/", "//", "/.", "/..", "/root/", "/root/.", "/root/..",
            "/a/../b", "/a//b", "/ro\0ot",
        ] {
            assert!(!is_usable_home(bad), "{bad:?}");
        }
    }

    /// The entry at uid 0 is the image's own, so the home it names is where the
    /// image put what root uses -- and the file is left as it is (#399).
    #[test]
    fn root_keeps_a_usable_home_and_its_entry_is_not_rewritten() {
        let debian = "root:x:0:0:root:/root:/bin/bash\n\
                      daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n";
        for (raw, home) in [
            (ALPINE_PASSWD.to_string(), "/root"),
            (debian.to_string(), "/root"),
            (root_entry("/var/lib/root"), "/var/lib/root"),
            // No shell field, with and without a line ending: still the sixth field.
            ("root:x:0:0:root:/var/root\r\n".to_string(), "/var/root"),
            ("root:x:0:0:root:/var/root".to_string(), "/var/root"),
        ] {
            let settled = settled_root(raw.as_bytes());
            assert_eq!(settled, (home.to_string(), raw.into_bytes()));
        }

        // The first entry at the uid counts, as it does for `lookup_id`.
        let raw = "# root:x:0:0::/commented:/bin/sh\n\
                   first:x:0:0::/one:/bin/sh\n\
                   second:x:0:0::/two:/bin/sh\n";
        assert_eq!(lookup_id(raw, 0).as_deref(), Some("first"));
        assert_eq!(
            settled_root(raw.as_bytes()),
            ("/one".to_string(), raw.as_bytes().to_vec())
        );
    }

    /// A root entry with no home that can be used is pointed at `/root`, the way
    /// a reused entry's home is pointed at the one bootstrap exports.
    #[test]
    fn root_without_a_usable_home_is_pointed_at_slash_root() {
        // Empty, relative, `/`, a `.`, `..` or empty part, or a trailing `/`.
        for home in [
            "", "root", "/", "//", "/root/", "/root/.", "/..", "/a/../b", "/ro\0ot",
        ] {
            assert_eq!(
                settled_root(root_entry(home).as_bytes()),
                ("/root".to_string(), root_entry("/root").into_bytes()),
                "{home:?}"
            );
        }

        // Not UTF-8, so not a home to make or export; the bytes around it stay.
        assert_eq!(
            settled_root(b"root:x:0:0:Jos\xe9:/r\xf6\xf6t:/bin/sh\n"),
            (
                "/root".to_string(),
                b"root:x:0:0:Jos\xe9:/root:/bin/sh\n".to_vec()
            )
        );

        // Too short to name a home at all: `/root`, though there is no field to
        // rewrite.
        assert_eq!(home(b"root:x:0:0:root\n", 0, "root"), "/root");
        assert_eq!(rehome(b"root:x:0:0:root\n", 0, "/root"), None);
    }

    /// With no account at uid 0 in the image, podman's `--user` handling plants
    /// one naming the working directory, which is the workspace -- under the
    /// host user's name or, failing that lookup, the uid. Neither is the image's
    /// home for root: it would put root's caches in the checkout, and
    /// `create_home` would `chown` the user's directory (#399).
    #[test]
    fn root_is_not_homed_in_the_workspace_podman_names_for_its_entry() {
        let before = format!(
            "bin:x:1:1:bin:/bin:/sbin/nologin\n{}",
            podman_keep_id_entry("/workspace")
        );
        for (planted, pointed) in [
            (
                "root:*:0:0:root:/workspace:/bin/sh\n",
                "root:*:0:0:root:/root:/bin/sh\n",
            ),
            (
                "0:*:0:0:container user:/workspace:/bin/sh\n",
                "0:*:0:0:container user:/root:/bin/sh\n",
            ),
        ] {
            let raw = format!("{before}{planted}");
            let want = format!("{before}{pointed}");
            assert_eq!(
                settled_root_in(raw.as_bytes(), Path::new("/workspace")),
                ("/root".to_string(), want.into_bytes()),
                "{planted:?}"
            );
        }
    }

    /// The workspace mounted at `/root` -- where a stock image's root entry
    /// points -- makes `/root` no better a home than the checkout is, whatever
    /// the entry names or lacks, so root is homed as any other user is (#399).
    #[test]
    fn root_is_homed_under_home_when_slash_root_is_the_workspace() {
        let workspace = Path::new("/root");
        assert_eq!(
            settled_root_in(root_entry("/root").as_bytes(), workspace),
            (
                "/home/root".to_string(),
                root_entry("/home/root").into_bytes()
            )
        );

        // No entry: the line bootstrap appends is for the same workspace, so it
        // already names `/home/root` and the file needs no rewriting.
        let raw = "bin:x:1:1:bin:/bin:/sbin/nologin\n";
        assert_eq!(home_dir(raw.as_bytes(), 0, "root", workspace), "/home/root");
        let after = format!("{raw}{}", passwd_line("root", 0, 0, workspace));
        assert_eq!(after, format!("{raw}root:x:0:0::/home/root:/bin/sh\n"));
        assert_eq!(
            settled_root_in(after.as_bytes(), workspace),
            ("/home/root".to_string(), after.into_bytes())
        );

        // The home is named for whoever is at uid 0.
        let raw = "0:*:0:0:container user:/root:/bin/sh\n";
        assert_eq!(home_dir(raw.as_bytes(), 0, "0", workspace), "/home/0");

        // An entry whose home is the image's own, and not the checkout, stands.
        let raw = root_entry("/home/dev");
        assert_eq!(home_dir(raw.as_bytes(), 0, "root", workspace), "/home/dev");
    }

    /// The path a workspace is spelled with is not the path it is mounted at:
    /// podman folds `.` and `..` out of a destination, so `/var/../root` is
    /// `/root`, and comparing the spelling would take the checkout for root's
    /// home -- `chown`ed to the container's root, or failing on a read-only
    /// mount (#399). With or without an entry, root is then homed under
    /// `/home`, as for the plain spelling.
    #[test]
    fn a_workspace_is_compared_as_podman_folds_it() {
        for spelling in [
            "/var/../root",
            "/./root",
            "//root",
            "/root/",
            "/a/b/../../root",
            "/root/x/..",
        ] {
            let workspace = Path::new(spelling);
            for raw in [root_entry("/root"), String::new()] {
                assert_eq!(
                    home_dir(raw.as_bytes(), 0, "root", workspace),
                    "/home/root",
                    "{spelling} with {raw:?}"
                );
            }
            // The line bootstrap appends for an image with no entry agrees.
            assert_eq!(
                passwd_line("root", 0, 0, workspace),
                "root:x:0:0::/home/root:/bin/sh\n",
                "{spelling}"
            );
        }

        // The entry's own home is held to the folded workspace too, at any depth.
        let raw = root_entry("/workspace/home");
        for spelling in ["/data/../workspace", "/workspace/.", "/workspace/a/.."] {
            assert_eq!(
                home_dir(raw.as_bytes(), 0, "root", Path::new(spelling)),
                "/root",
                "{spelling}"
            );
        }

        // A spelling that folds to somewhere else leaves `/root` alone.
        for spelling in ["/var/../root2", "/root/../workspace", "/root/a/../b"] {
            assert_eq!(
                home_dir(ALPINE_PASSWD.as_bytes(), 0, "root", Path::new(spelling)),
                "/root",
                "{spelling}"
            );
        }
    }

    /// A workspace below or beside `/root` is not `/root`, so it stays out of
    /// its way: with or without an entry, root is at `/root`, file as it was.
    #[test]
    fn a_workspace_below_or_beside_slash_root_leaves_it_alone() {
        for workspace in ["/root/project", "/root2", "/rooted", "/workspace"] {
            for raw in [root_entry("/root"), String::new()] {
                let settled = settled_root_in(raw.as_bytes(), Path::new(workspace));
                assert_eq!(
                    settled,
                    ("/root".to_string(), raw.into_bytes()),
                    "{workspace}"
                );
            }
        }
    }

    /// A home is out when it is the workspace or inside it at any depth, not
    /// when it merely shares a prefix with it or sits above it. The spelling of
    /// the workspace's path does not matter. With no workspace there is nothing
    /// to keep out of, so the entry's home stands.
    #[test]
    fn a_root_home_is_out_only_when_it_is_the_workspace_or_inside_it() {
        for (home, kept) in [
            ("/workspace", false),
            ("/workspace/home", false),
            ("/workspace/a/b", false),
            ("/workspace/.cache", false),
            ("/workspace2", true),
            ("/workspace-old", true),
            ("/work", true),
            ("/var/workspace", true),
            ("/root", true),
        ] {
            let (settled, _) =
                settled_root_in(root_entry(home).as_bytes(), Path::new("/workspace"));
            assert_eq!(settled, if kept { home } else { "/root" }, "{home}");
        }

        let raw = root_entry("/workspace");
        assert_eq!(
            home_dir(raw.as_bytes(), 0, "root", Path::new("/workspace/")),
            "/root"
        );
        // A home the workspace is below is the image's own.
        let raw = root_entry("/home/dev");
        let workspace = Path::new("/home/dev/project");
        assert_eq!(home_dir(raw.as_bytes(), 0, "root", workspace), "/home/dev");

        let raw = root_entry("/workspace");
        assert_eq!(
            settled_root(raw.as_bytes()),
            ("/workspace".to_string(), raw.into_bytes())
        );
    }

    /// With no entry at uid 0 the line bootstrap appends names `/root`, and the
    /// file then agrees with the home `home_dir` settles on.
    #[test]
    fn root_with_no_entry_is_appended_naming_slash_root() {
        let raw = "bin:x:1:1:bin:/bin:/sbin/nologin\n";
        assert_eq!(home(raw.as_bytes(), 0, "root"), "/root");

        let after = format!("{raw}{}", passwd_line("root", 0, 0, Path::new("")));
        assert_eq!(
            settled_root(after.as_bytes()),
            ("/root".to_string(), after.into_bytes())
        );
    }

    /// `raw` with `rehome`'s rewrite applied, as the truncate-and-append
    /// through the file's descriptor would leave it.
    fn applied(raw: &[u8], rehome: &Rehome) -> Vec<u8> {
        let mut out = raw[..rehome.at].to_vec();
        out.extend_from_slice(&rehome.tail);
        out
    }

    /// What podman's bare `--userns=keep-id` appends: a `*` password, the host
    /// user's GECOS, and the container's working directory as the home.
    fn podman_keep_id_entry(home: &str) -> String {
        format!("tgockel:*:1000:1000:Travis Gockel:{home}:/bin/sh\n")
    }

    #[test]
    fn rehome_changes_only_the_home_of_podmans_entry() {
        let raw = format!("{ALPINE_PASSWD}{}", podman_keep_id_entry("/workspace"));
        let rehome = rehome(raw.as_bytes(), 1000, "/home/tgockel").expect("rehome");

        assert_eq!(rehome.at, ALPINE_PASSWD.len());
        assert_eq!(rehome.was, "/workspace");
        let expected = format!("{ALPINE_PASSWD}{}", podman_keep_id_entry("/home/tgockel"));
        assert_eq!(applied(raw.as_bytes(), &rehome), expected.as_bytes());
    }

    #[test]
    fn rehome_carries_the_lines_after_the_entry_verbatim() {
        let raw = "root:x:0:0:root:/root:/bin/ash\n\
                   tgockel:x:1000:1000::/:/bin/sh\n\
                   # a comment\n\
                   after:x:4242:4242::/after:/bin/sh\n";
        let rehome = rehome(raw.as_bytes(), 1000, "/home/tgockel").expect("rehome");
        assert_eq!(
            String::from_utf8(rehome.tail).unwrap(),
            "tgockel:x:1000:1000::/home/tgockel:/bin/sh\n\
             # a comment\n\
             after:x:4242:4242::/after:/bin/sh\n"
        );
    }

    #[test]
    fn rehome_is_none_when_nothing_needs_rewriting() {
        let agreeing = format!("{ALPINE_PASSWD}{}", podman_keep_id_entry("/home/tgockel"));
        assert_eq!(rehome(agreeing.as_bytes(), 1000, "/home/tgockel"), None);
        assert_eq!(
            rehome(ALPINE_PASSWD.as_bytes(), 1000, "/home/tgockel"),
            None
        );
        // Too short to have a home field to rewrite.
        assert_eq!(rehome(b"tgockel:x:1000\n", 1000, "/home/tgockel"), None);
        assert_eq!(
            rehome(b"tgockel:x:1000:1000:gecos\n", 1000, "/home/tgockel"),
            None
        );
    }

    /// glibc reads a line with no shell field as having an empty shell, and
    /// its last field as the home -- so that is the field to rewrite, without
    /// taking the terminator along with it.
    #[test]
    fn rehome_rewrites_a_home_with_no_shell_after_it() {
        for (raw, expected) in [
            (
                &b"dev:x:1000:1000::/workspace\n"[..],
                &b"dev:x:1000:1000::/home/dev\n"[..],
            ),
            (
                b"dev:x:1000:1000::/workspace\r\n",
                b"dev:x:1000:1000::/home/dev\r\n",
            ),
            (
                b"dev:x:1000:1000::/workspace",
                b"dev:x:1000:1000::/home/dev",
            ),
        ] {
            let rehome = rehome(raw, 1000, "/home/dev").expect("rehome");
            assert_eq!(rehome.was, "/workspace");
            assert_eq!(applied(raw, &rehome), expected);
        }
        assert_eq!(
            rehome(b"dev:x:1000:1000::/home/dev\n", 1000, "/home/dev"),
            None
        );
    }

    /// The line rewritten is the one `lookup_id` resolved the name from.
    #[test]
    fn rehome_rewrites_the_entry_lookup_id_finds() {
        let raw = "# tgockel:x:1000:1000::/commented:/bin/sh\n\
                   first:x:1000:1000::/one:/bin/sh\n\
                   second:x:1000:1000::/two:/bin/sh\n";
        assert_eq!(lookup_id(raw, 1000).as_deref(), Some("first"));
        let rehome = rehome(raw.as_bytes(), 1000, "/home/first").expect("rehome");
        assert_eq!(rehome.was, "/one");
        assert!(applied(raw.as_bytes(), &rehome).ends_with(
            b"first:x:1000:1000::/home/first:/bin/sh\nsecond:x:1000:1000::/two:/bin/sh\n"
        ));
    }

    #[test]
    fn rehome_keeps_the_bytes_it_does_not_own() {
        // No trailing newline on the entry, a CRLF, and a Latin-1 GECOS -- the
        // file is only ever read lossily, so none of these may round-trip
        // through a `String`.
        let raw = b"root:x:0:0:root:/root:/bin/ash\r\n\
                    other:x:7:7:Jos\xe9:/o:/bin/sh\n\
                    tgockel:x:1000:1000:Andr\xe9:/workspace:/bin/sh";
        let rehome = rehome(raw, 1000, "/home/tgockel").expect("rehome");
        assert_eq!(
            applied(raw, &rehome),
            b"root:x:0:0:root:/root:/bin/ash\r\n\
              other:x:7:7:Jos\xe9:/o:/bin/sh\n\
              tgockel:x:1000:1000:Andr\xe9:/home/tgockel:/bin/sh"
        );
    }
}
