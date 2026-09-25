//! Parsing and formatting for the container's `/etc/passwd` and `/etc/group`.
//!
//! Everything here is pure: no podman, no namespaces, no I/O. It is the half
//! of [`Container::bootstrap_user`] that used to be delegated to `getent`,
//! `groupadd`, and `useradd` running inside the image.
//!
//! [`Container::bootstrap_user`]: super::Container::bootstrap_user

use std::borrow::Cow;

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

/// The in-container home directory for `name`. The one definition: it is
/// written into `/etc/passwd` -- or, for an entry reused from the image or
/// planted by podman, rewritten there by [`rehome`] -- created and `chown`ed
/// after that, and passed as `HOME` to every `podman exec`. All three have to
/// agree.
pub(super) fn home_dir(name: &str) -> String {
    format!("/home/{name}")
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
    let entry =
        entries(raw).find(|entry| entry_id(&entry.text).is_some_and(|(_, id)| id == uid))?;
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
/// `home` must agree with the `HOME` that
/// [`Container::build_exec_argv`](super::Container::build_exec_argv) passes to
/// every `podman exec`.
pub(super) fn passwd_line(name: &str, uid: u32, gid: u32, home: &str) -> String {
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
            text.push_str(&passwd_line(&name, 100 + i, 100 + i, "/home/x"));
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
            passwd_line("tgockel", 1000, 1000, "/home/tgockel"),
            "tgockel:x:1000:1000::/home/tgockel:/bin/sh\n"
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
        let line = passwd_line("tgockel", 1000, 1000, "/home/tgockel");
        let mut text = ALPINE_PASSWD.to_string();
        text.push_str(&String::from_utf8(append_blob(&text, &line)).unwrap());

        assert_eq!(lookup_id(&text, 1000).as_deref(), Some("tgockel"));
        assert_eq!(lookup_id(&text, 0).as_deref(), Some("root"));
        assert!(name_taken(&text, "tgockel"));
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
