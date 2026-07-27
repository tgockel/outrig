//! Parsing and formatting for the container's `/etc/passwd` and `/etc/group`.
//!
//! Everything here is pure: no podman, no namespaces, no I/O. It is the half
//! of [`Container::bootstrap_user`] that used to be delegated to `getent`,
//! `groupadd`, and `useradd` running inside the image.
//!
//! [`Container::bootstrap_user`]: super::Container::bootstrap_user

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
    entry_lines(text).find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let found = fields.nth(1)?.parse::<u32>().ok()?;
        (found == id).then(|| name.to_string())
    })
}

/// Whether `name` already occupies the first field of some entry.
pub(super) fn name_taken(text: &str, name: &str) -> bool {
    entry_lines(text).any(|line| line.split(':').next() == Some(name))
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
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

/// The in-container home directory for `name`. The one definition: it is
/// written into `/etc/passwd`, created and `chown`ed after that, and passed as
/// `HOME` to every `podman exec` -- all three have to agree.
pub(super) fn home_dir(name: &str) -> String {
    format!("/home/{name}")
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

/// Lines that carry an entry, skipping what `nss_files` skips: blank lines and
/// comments. Malformed lines fall out of the lookups themselves, which need a
/// name and a parseable id.
fn entry_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
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
}
