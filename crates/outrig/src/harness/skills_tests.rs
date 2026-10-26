//! Skills: the metadata discovery reads with no Python, the directory source and what it refuses,
//! discovery across sources, the host's answers on the wire, and the loader in an interpreter on
//! the host, which fetches a skill's files as they are used.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;
use crate::events::StreamBuilder;
use crate::harness::event::{DEFAULT_CAPACITY, Payload, Received, Subscription};
use crate::python::host::{Interpreter, Outcome};
use crate::python::testing::{Fake, py, start_on_host_with, within};

/// How long a test lets a source take: long enough for a loaded machine, short enough that a test
/// of a source that never answers ends quickly.
const BOUND: Duration = Duration::from_secs(10);

/// A directory of skills, removed with the test.
struct Root(tempfile::TempDir);

impl Root {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("a temporary directory"))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    /// Skill `name`'s directory, holding `files`: each a path and what it holds.
    fn skill(&self, name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = self.path().join(name);
        fs::create_dir_all(&dir).expect("the skill's directory");
        for (path, text) in files {
            let path = dir.join(path);
            fs::create_dir_all(path.parent().expect("a parent")).expect("a directory");
            fs::write(&path, text).expect("a file");
        }
        dir
    }

    fn source(&self) -> Arc<dyn SkillSource> {
        Arc::new(DirectorySource::new(self.path()))
    }
}

/// A `SKILL.md` naming `name`, described by `description`.
fn instructions(name: &str, description: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\nFollow the steps.\n")
}

/// A source that serves another's skills and logs each path it is asked to read.
struct Counting {
    inner: Arc<dyn SkillSource>,
    reads: Mutex<Vec<String>>,
}

impl Counting {
    fn new(inner: Arc<dyn SkillSource>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: Mutex::default(),
        })
    }

    /// How often `path` of `skill` has been read.
    fn count(&self, skill: &str, path: &str) -> usize {
        let wanted = format!("{skill}/{path}");
        lock(&self.reads)
            .iter()
            .filter(|read| **read == wanted)
            .count()
    }
}

impl SkillSource for Counting {
    fn skills(&self) -> io::Result<Vec<String>> {
        self.inner.skills()
    }

    fn files(&self, skill: &str) -> io::Result<Vec<SkillFile>> {
        self.inner.files(skill)
    }

    fn read(&self, skill: &str, path: &str) -> io::Result<Vec<u8>> {
        lock(&self.reads).push(format!("{skill}/{path}"));
        self.inner.read(skill, path)
    }
}

/// `sources`, named.
fn named(sources: Vec<(&str, Arc<dyn SkillSource>)>) -> Vec<(String, Arc<dyn SkillSource>)> {
    sources
        .into_iter()
        .map(|(name, source)| (name.to_string(), source))
        .collect()
}

/// A stream of events, and a subscription to it.
fn stream() -> (Events, Subscription) {
    let mut stream = StreamBuilder::default();
    let subscription = stream.subscribe(DEFAULT_CAPACITY);
    (stream.build(), subscription)
}

/// The events `subscription` holds now.
fn held(subscription: &mut Subscription) -> Vec<Arc<crate::harness::event::Event>> {
    let mut events = Vec::new();
    while let Ok(Received::Event(event)) = subscription.try_recv() {
        events.push(event);
    }
    events
}

/// What discovering `sources` found: the catalog, and what `skill.discovered` said.
async fn discovered(
    sources: Vec<(&str, Arc<dyn SkillSource>)>,
) -> (Arc<Catalog>, Vec<DiscoveredSkill>, Vec<SkillDiagnostic>) {
    let (events, mut subscription) = stream();
    let catalog = discover(named(sources), &events, BOUND).await;
    let events = held(&mut subscription);
    let [event] = &events[..] else {
        panic!("one event, not {events:?}");
    };
    let Payload::SkillDiscovered {
        skills,
        diagnostics,
    } = &event.payload
    else {
        panic!("skill.discovered, not {event:?}");
    };
    (catalog, skills.clone(), diagnostics.clone())
}

/// Each diagnostic as `code skill source`.
fn codes(diagnostics: &[SkillDiagnostic]) -> Vec<String> {
    diagnostics
        .iter()
        .map(|d| {
            format!(
                "{} {} {}",
                d.code,
                d.skill.as_deref().unwrap_or("-"),
                d.source
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------- what is read

#[test]
fn a_skill_name_follows_the_standard() {
    for name in ["a", "review-diff", "7zip", "pdf-2-text", &"x".repeat(64)] {
        assert!(is_skill_name(name), "{name}");
    }
    for name in [
        "",
        "-pdf",
        "pdf-",
        "pdf--processing",
        "PDF",
        "pdf_text",
        "pdf.text",
        "über",
        &"x".repeat(65),
    ] {
        assert!(!is_skill_name(name), "{name}");
    }
}

#[test]
fn frontmatter_is_what_lies_between_the_first_two_dashed_lines() {
    assert_eq!(
        split("---\nname: a\n---\nbody\n---\nmore\n"),
        Some(("name: a\n", "body\n---\nmore\n"))
    );
    assert_eq!(split("---  \nname: a\n---\t\n"), Some(("name: a\n", "")));
    assert_eq!(split("---\n---\nbody"), Some(("", "body")));
    assert_eq!(split("name: a\n---\n"), None);
    assert_eq!(split("---\nname: a\n"), None);
    assert_eq!(split("--- \n"), None);
}

#[test]
fn frontmatter_gives_a_name_and_description_in_any_scalar_style() {
    let read = |yaml: &str| frontmatter(yaml).unwrap_or_else(|e| panic!("{yaml}: {e}"));
    let both = |name: &str, description: &str| (Some(name.into()), Some(description.into()));
    assert_eq!(
        read("name: a\ndescription: Does b.\n"),
        both("a", "Does b.")
    );
    assert_eq!(
        read("name: 'a'\ndescription: \"Does \\\"b\\\".\"\n"),
        both("a", "Does \"b\".")
    );
    assert_eq!(
        read("name: a\ndescription: >-\n  Does b,\n  then c.\n"),
        both("a", "Does b, then c.")
    );
    assert_eq!(
        read("name: a\ndescription: |\n  One.\n  Two.\n"),
        both("a", "One.\nTwo.\n")
    );
    assert_eq!(
        read("name: a\ndescription: Spans\n  two lines.\n"),
        both("a", "Spans two lines.")
    );
    // Every other key is read as YAML and ignored, whatever its shape: another tool's fields.
    assert_eq!(
        read(
            "name: a\ndescription: d\nallowed-tools: [Read, Grep]\nmetadata:\n  version: \"1.0\"\n\
             hooks:\n  PreToolUse:\n    - matcher: Bash\n      hooks: [{type: command}]\n\
             disable-model-invocation: yes\n"
        ),
        both("a", "d")
    );
    assert_eq!(read("name: a\ndescription:\n"), (Some("a".into()), None));
    assert_eq!(read("name: a\ndescription: ~\n"), (Some("a".into()), None));
    assert_eq!(read("name: a\ndescription: 'null'\n"), both("a", "null"));
    assert_eq!(read(""), (None, None));
}

#[test]
fn frontmatter_refuses_anchors_aliases_and_what_is_not_one_flat_mapping() {
    for (yaml, why) in [
        ("name: &n a\ndescription: d\n", "anchor"),
        ("name: a\nmeta: &m {x: 1}\ndescription: d\n", "anchor"),
        ("x: &x [1]\ny: *x\nname: a\n", "anchor"),
        ("name: a\ndescription: *d\n", "not valid YAML"),
        ("name: a\nname: b\n", "twice"),
        ("- name: a\n", "not a mapping"),
        ("just text\n", "not a mapping"),
        ("name: [a]\n", "name is not a string"),
        ("description: {a: b}\n", "description is not a string"),
        ("? [a, b]\n: c\n", "not a scalar"),
        ("name: a\n...\n---\nname: b\n", "more than one"),
        (
            "argument-hint: [filename] [format]\nname: a\n",
            "not valid YAML",
        ),
        ("name: 'unclosed\n", "not valid YAML"),
    ] {
        let refused = frontmatter(yaml).expect_err(yaml);
        assert!(refused.contains(why), "{yaml:?}: {refused}");
    }
}

#[test]
fn frontmatter_nesting_is_bounded() {
    let deep =
        |levels: usize| format!("name: a\nx: {}{}\n", "[".repeat(levels), "]".repeat(levels));
    assert!(frontmatter(&deep(NESTING_MAX - 1)).is_ok());
    let refused = frontmatter(&deep(NESTING_MAX)).expect_err("too deep");
    assert!(refused.contains("deeper than"), "{refused}");
    // Far past any parser's own limit, and still an answer rather than a crash.
    assert!(frontmatter(&deep(1 << 16)).is_err());
}

/// The block is read only from the lines before the module's first statement.
#[test]
fn metadata_is_read_only_before_the_code() {
    let read = |source: &str| {
        let mut warnings = Vec::new();
        script(source, &mut warnings).map(|needs| (needs, warnings))
    };
    let (declared, _) = read(
        "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n\n# /// script\n\
         # requires-python = \">=3.13\"\n# dependencies = [\"unidiff>=0.7\"]\n#\n\
         # [tool.outrig.skill]\n# entry = \"run\"\n# bind = { repo = \"repo\" }\n#\n\
         # [tool.outrig.skill.parameters]\n# base = \"The ref to diff against.\"\n# ///\n\
         import os\n",
    )
    .expect("a block before the code");
    assert_eq!(declared.requires_python.as_deref(), Some(">=3.13"));
    assert_eq!(declared.dependencies, ["unidiff>=0.7"]);
    assert_eq!(declared.entry.as_deref(), Some("run"));
    let pair = |key: &str, value: &str| (key.to_string(), value.to_string());
    assert_eq!(declared.bind, BTreeMap::from([pair("repo", "repo")]));
    assert_eq!(
        declared.parameters,
        BTreeMap::from([pair("base", "The ref to diff against.")])
    );

    for ignored in [
        // After the first statement.
        "import os\n# /// script\n# requires-python = \">=3.99\"\n# ///\n",
        // In a docstring, which is a statement.
        "\"\"\"A skill.\n\n# /// script\n# requires-python = \">=3.99\"\n# ///\n\"\"\"\n",
        // Never closed.
        "# /// script\n# requires-python = \">=3.99\"\nimport os\n",
        // Of a type the specification has not standardized.
        "# /// pyproject\n# requires-python = \">=3.99\"\n# ///\n",
    ] {
        let (declared, warnings) = read(ignored).expect(ignored);
        assert_eq!(declared, Declared::default(), "{ignored}");
        assert!(warnings.is_empty(), "{ignored}");
    }

    let two =
        "# /// script\n# dependencies = []\n# ///\n\n# /// script\n# dependencies = []\n# ///\n";
    let refused = read(two).expect_err("two blocks");
    assert_eq!(refused.code, "duplicate-metadata-block");
    // Two blocks with no line between them are one block, whose `///` lines are not TOML.
    let adjacent = "# /// script\n# dependencies = []\n# ///\n# /// script\n# x = 1\n# ///\n";
    assert_eq!(
        read(adjacent).expect_err("not TOML").code,
        "metadata-invalid"
    );
}

#[test]
fn the_block_takes_entry_bind_and_parameters_and_warns_of_any_other_key() {
    let read = |table: &str| {
        let mut warnings = Vec::new();
        let source: String = table.lines().map(|line| format!("# {line}\n")).collect();
        script(&format!("# /// script\n{source}# ///\n"), &mut warnings).map(|_| warnings)
    };
    assert_eq!(
        read(
            "[tool.outrig.skill]\nentry = \"run\"\nbind = { repo = \"repo\" }\n\
             [tool.outrig.skill.parameters]\nbase = \"The ref.\""
        )
        .expect("valid"),
        Vec::<String>::new()
    );
    let warnings = read("[tool.outrig.skill]\nentrypoint = \"run\"\n[tool.outrig.other]\nx = 1")
        .expect("warned, not refused");
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("\"other\"") || warnings[1].contains("\"other\""));
    assert!(
        warnings.iter().any(|w| w.contains("\"entrypoint\"")),
        "{warnings:?}"
    );
    // Another tool's table is its own.
    assert_eq!(
        read("[tool.uv]\nexclude-newer = \"2026-01-01\"").expect("valid"),
        Vec::<String>::new()
    );
    for refused in [
        "requires-python = 3",
        "dependencies = \"unidiff\"",
        "dependencies = [1]",
        "tool = 1",
        "[tool.outrig.skill]\nentry = 1",
        "[tool.outrig.skill]\nbind = { repo = 1 }",
        "[tool.outrig.skill]\nbind = \"repo\"",
        "[tool.outrig.skill.parameters]\nbase = 1",
        "[tool.outrig.skill]\nparameters = \"base\"",
        "not toml at all",
    ] {
        let problem = read(refused).expect_err(refused);
        assert_eq!(problem.code, "metadata-invalid", "{refused}: {problem:?}");
    }
}

// ---------------------------------------------------------------------------- the directory source

#[test]
fn a_directory_source_lists_its_skills_and_their_files() {
    let root = Root::new();
    root.skill(
        "review",
        &[
            ("SKILL.md", "x"),
            ("skill.py", "x"),
            ("lib/sections.py", "x"),
            ("data/ids.txt", "x"),
            (".hidden", "x"),
            ("__pycache__/skill.cpython-313.pyc", "x"),
            ("lib/__pycache__/sections.cpython-313.pyc", "x"),
        ],
    );
    root.skill("other", &[("SKILL.md", "x")]);
    root.skill(".git", &[("HEAD", "x")]);
    fs::write(root.path().join("README.md"), "x").expect("a file beside the skills");
    let source = DirectorySource::new(root.path());
    assert_eq!(source.skills().expect("listed"), ["other", "review"]);
    let paths: Vec<String> = source
        .files("review")
        .expect("listed")
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(
        paths,
        ["SKILL.md", "data/ids.txt", "lib/sections.py", "skill.py"]
    );
    assert_eq!(
        source.read("review", "lib/sections.py").expect("read"),
        b"x"
    );

    assert!(
        DirectorySource::new(root.path().join("absent"))
            .skills()
            .expect("no root, no skills")
            .is_empty()
    );
}

#[test]
fn a_files_version_changes_with_it() {
    let root = Root::new();
    let dir = root.skill("a", &[("SKILL.md", "one")]);
    let source = DirectorySource::new(root.path());
    let before = source.files("a").expect("listed");
    fs::write(dir.join("SKILL.md"), "three").expect("rewritten");
    let after = source.files("a").expect("listed");
    assert_ne!(before, after);
    assert_ne!(digest(&before), digest(&after));
    assert_eq!(digest(&after), digest(&source.files("a").expect("listed")));
}

#[test]
fn a_directory_source_follows_no_link_below_its_root() {
    let outside = Root::new();
    fs::write(outside.path().join("secret.txt"), "secret").expect("a host file");
    fs::create_dir(outside.path().join("dir")).expect("a host directory");
    fs::write(outside.path().join("dir/inner.txt"), "secret").expect("a host file");
    let root = Root::new();
    let dir = root.skill("a", &[("SKILL.md", "x")]);
    symlink(outside.path().join("secret.txt"), dir.join("secret.txt")).expect("a link out");
    symlink(outside.path().join("dir"), dir.join("dir")).expect("a link out");
    symlink("SKILL.md", dir.join("again.md")).expect("a link within");
    nix::unistd::mkfifo(&dir.join("fifo"), nix::sys::stat::Mode::S_IRWXU).expect("a FIFO");
    symlink(outside.path(), root.path().join("linked")).expect("a linked skill directory");

    let source = DirectorySource::new(root.path());
    // A link is listed, so reading it fails saying why rather than finding nothing.
    let paths: Vec<String> = source
        .files("a")
        .expect("listed")
        .into_iter()
        .map(|file| file.path)
        .collect();
    assert_eq!(paths, ["SKILL.md", "again.md", "dir", "fifo", "secret.txt"]);
    for (path, why) in [
        ("secret.txt", "symbolic link"),
        ("again.md", "symbolic link"),
        ("dir/inner.txt", "symbolic link"),
        ("fifo", "not a regular file"),
        ("../a/SKILL.md", "cannot name"),
        ("/etc/hostname", "cannot name"),
        ("./SKILL.md", "cannot name"),
        ("data//x", "cannot name"),
        (".hidden", "cannot name"),
    ] {
        let refused = source.read("a", path).expect_err(path).to_string();
        assert!(refused.contains(why), "{path}: {refused}");
    }
    let refused = source
        .files("linked")
        .expect_err("a linked skill")
        .to_string();
    assert!(refused.contains("symbolic link"), "{refused}");
    assert!(source.files("..").is_err());
}

#[test]
fn a_root_within_its_base_may_be_a_link_inside_it_and_never_one_out() {
    let repo = Root::new();
    let skills = repo.path().join(".claude/skills");
    fs::create_dir_all(skills.join("a")).expect("a skills directory");
    fs::write(skills.join("a/SKILL.md"), "x").expect("a skill");
    fs::create_dir(repo.path().join(".agents")).expect(".agents");
    symlink("../.claude/skills", repo.path().join(".agents/skills")).expect("a link inside");
    let inside = DirectorySource::within(repo.path(), ".agents/skills");
    assert_eq!(inside.skills().expect("listed"), ["a"]);

    let elsewhere = Root::new();
    elsewhere.skill("b", &[("SKILL.md", "x")]);
    let other = Root::new();
    fs::create_dir(other.path().join(".agents")).expect(".agents");
    symlink(elsewhere.path(), other.path().join(".agents/skills")).expect("a link out");
    let out = DirectorySource::within(other.path(), ".agents/skills");
    let refused = out
        .skills()
        .expect_err("outside the repository")
        .to_string();
    assert!(refused.contains("outside"), "{refused}");
    // Not a root it falls back from: the next listing is refused the same way.
    assert!(out.files("b").is_err());

    assert!(
        DirectorySource::within(other.path(), "absent")
            .skills()
            .expect("no skills")
            .is_empty()
    );
}

/// The root is the directory found at the first listing, whatever is put at its path later.
#[test]
fn the_root_is_held_from_its_first_use() {
    let parent = Root::new();
    let root = parent.path().join("skills");
    fs::create_dir_all(root.join("a")).expect("a skill");
    fs::write(root.join("a/SKILL.md"), "first").expect("a skill");
    let source = DirectorySource::new(&root);
    assert_eq!(source.skills().expect("listed"), ["a"]);

    let elsewhere = Root::new();
    elsewhere.skill("a", &[("SKILL.md", "elsewhere")]);
    fs::rename(&root, parent.path().join("moved")).expect("moved away");
    symlink(elsewhere.path(), &root).expect("replaced by a link");
    assert_eq!(source.read("a", "SKILL.md").expect("read"), b"first");
}

#[test]
fn a_directory_source_bounds_what_it_serves() {
    let root = Root::new();
    let dir = root.skill("a", &[("SKILL.md", "x")]);
    let large = File::create(dir.join("large.bin")).expect("a file");
    large.set_len(FILE_MAX + 1).expect("sparse, past the bound");
    let source = DirectorySource::new(root.path());
    let refused = source
        .read("a", "large.bin")
        .expect_err("too large")
        .to_string();
    assert!(refused.contains("larger than"), "{refused}");

    let crowded = root.skill("b", &[("SKILL.md", "x")]);
    for i in 0..ENTRIES_MAX {
        fs::write(crowded.join(format!("f{i}")), "").expect("a file");
    }
    let refused = source.files("b").expect_err("too many").to_string();
    assert!(refused.contains("more than"), "{refused}");

    let deep = root.skill("c", &[("SKILL.md", "x")]);
    let nested: PathBuf = (0..DEPTH_MAX).map(|i| format!("d{i}")).collect();
    fs::create_dir_all(deep.join(&nested)).expect("deep directories");
    fs::write(deep.join(&nested).join("f"), "").expect("the deepest file");
    let files = source.files("c").expect("as deep as a skill may go");
    assert_eq!(files.len(), 2, "{files:?}");
    fs::create_dir(deep.join(&nested).join("one-more")).expect("one deeper");
    let refused = source.files("c").expect_err("too deep").to_string();
    assert!(refused.contains("deeper than"), "{refused}");
}

// ---------------------------------------------------------------------------- discovery

#[tokio::test]
async fn the_first_source_serves_a_name_and_a_later_copy_is_reported_unread() {
    let project = Root::new();
    project.skill(
        "review",
        &[("SKILL.md", &instructions("review", "The project's."))],
    );
    let user = Root::new();
    user.skill(
        "review",
        &[("SKILL.md", &instructions("review", "The user's."))],
    );
    user.skill(
        "notes",
        &[("SKILL.md", &instructions("notes", "Takes notes."))],
    );
    let counted = Counting::new(user.source());
    let (catalog, skills, diagnostics) = discovered(vec![
        ("project", project.source()),
        ("user", counted.clone()),
    ])
    .await;
    let served: Vec<(String, String, String)> = skills
        .iter()
        .map(|s| (s.name.clone(), s.source.clone(), s.description.clone()))
        .collect();
    assert_eq!(
        served,
        [
            ("review".into(), "project".into(), "The project's.".into()),
            ("notes".into(), "user".into(), "Takes notes.".into()),
        ]
    );
    assert_eq!(codes(&diagnostics), ["shadowed review user"]);
    assert!(
        diagnostics[0].message.contains("the \"project\" source"),
        "{diagnostics:?}"
    );
    assert_eq!(
        counted.count("review", "SKILL.md"),
        0,
        "the shadowed copy was read"
    );
    assert_eq!(
        catalog.briefs(),
        [
            SkillBrief {
                name: "review".into(),
                description: "The project's.".into(),
                package: None,
            },
            SkillBrief {
                name: "notes".into(),
                description: "Takes notes.".into(),
                package: None,
            },
        ]
    );
}

#[tokio::test]
async fn discovery_reads_skill_md_and_skill_py_and_nothing_else() {
    let root = Root::new();
    root.skill(
        "review",
        &[
            ("SKILL.md", &instructions("review", "Reviews.")),
            ("skill.py", "VALUE = 1\n"),
            ("sections.py", "x = 1\n"),
            ("data/ids.txt", "a\n"),
        ],
    );
    let counted = Counting::new(root.source());
    let (_, skills, diagnostics) = discovered(vec![("project", counted.clone())]).await;
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(skills[0].executable);
    assert_eq!(
        *lock(&counted.reads),
        ["review/SKILL.md", "review/skill.py"],
        "discovery read only the metadata"
    );
}

#[tokio::test]
async fn what_is_not_a_valid_skill_is_left_out_and_said() {
    let root = Root::new();
    root.skill("mismatch", &[("SKILL.md", &instructions("other", "d"))]);
    root.skill("Upper", &[("SKILL.md", &instructions("Upper", "d"))]);
    root.skill("bare", &[("README.md", "no SKILL.md")]);
    root.skill("plain", &[("SKILL.md", "# no frontmatter\n")]);
    root.skill("nameless", &[("SKILL.md", "---\ndescription: d\n---\n")]);
    root.skill("silent", &[("SKILL.md", "---\nname: silent\n---\n")]);
    root.skill(
        "long",
        &[("SKILL.md", &instructions("long", &"d".repeat(1025)))],
    );
    root.skill(
        "aliased",
        &[("SKILL.md", "---\nname: &n aliased\ndescription: d\n---\n")],
    );
    root.skill(
        "twice",
        &[
            ("SKILL.md", &instructions("twice", "d")),
            (
                "skill.py",
                "# /// script\n# x = 1\n# ///\n\n# /// script\n# y = 1\n# ///\n",
            ),
        ],
    );
    root.skill(
        "warned",
        &[
            ("SKILL.md", &instructions("warned", "d")),
            (
                "skill.py",
                "# /// script\n# [tool.outrig.skill]\n# entrypoint = \"run\"\n# ///\n",
            ),
        ],
    );
    root.skill(
        "crlf",
        &[(
            "SKILL.md",
            "\u{feff}---\r\nname: crlf\r\ndescription: d\r\n---\r\n",
        )],
    );
    let outside = Root::new();
    outside.skill("linked", &[("SKILL.md", &instructions("linked", "d"))]);
    symlink(outside.path().join("linked"), root.path().join("linked")).expect("a link");

    let (_, skills, diagnostics) = discovered(vec![("project", root.source())]).await;
    let served: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(served, ["crlf", "warned"]);
    let mut codes = codes(&diagnostics);
    codes.sort();
    assert_eq!(
        codes,
        [
            "duplicate-metadata-block twice project",
            "metadata-invalid Upper project",
            "metadata-invalid aliased project",
            "metadata-invalid bare project",
            "metadata-invalid long project",
            "metadata-invalid mismatch project",
            "metadata-invalid nameless project",
            "metadata-invalid plain project",
            "metadata-invalid silent project",
            "source-error linked project",
            "unknown-key warned project",
        ]
    );
    let said = |skill: &str| {
        diagnostics
            .iter()
            .find(|d| d.skill.as_deref() == Some(skill))
            .map(|d| d.message.clone())
            .expect("a diagnostic")
    };
    assert!(
        said("mismatch").contains("\"other\", is not its directory's"),
        "{}",
        said("mismatch")
    );
    assert!(said("long").contains("1025 characters"), "{}", said("long"));
    assert!(
        said("warned").contains("\"entrypoint\""),
        "{}",
        said("warned")
    );
    assert!(
        said("linked").contains("symbolic link"),
        "{}",
        said("linked")
    );
}

/// A source that holds one skill and never answers a read of it.
struct Stuck {
    released: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Stuck {
    fn wait(&self) {
        let (released, cond) = &*self.released;
        let mut released = lock(released);
        while !*released {
            released = cond.wait(released).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

impl Drop for Stuck {
    fn drop(&mut self) {
        *lock(&self.released.0) = true;
        self.released.1.notify_all();
    }
}

impl SkillSource for Stuck {
    fn skills(&self) -> io::Result<Vec<String>> {
        Ok(vec!["stuck".into()])
    }

    fn files(&self, _: &str) -> io::Result<Vec<SkillFile>> {
        Ok(vec![SkillFile::new("SKILL.md", "1")])
    }

    fn read(&self, _: &str, _: &str) -> io::Result<Vec<u8>> {
        self.wait();
        Ok(Vec::new())
    }
}

/// A source that cannot list its skills.
struct Broken;

impl SkillSource for Broken {
    fn skills(&self) -> io::Result<Vec<String>> {
        Err(io::Error::other("the service is down"))
    }

    fn files(&self, _: &str) -> io::Result<Vec<SkillFile>> {
        unreachable!()
    }

    fn read(&self, _: &str, _: &str) -> io::Result<Vec<u8>> {
        unreachable!()
    }
}

#[tokio::test]
async fn a_source_that_fails_or_never_answers_is_reported_and_the_rest_served() {
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let stuck: Arc<dyn SkillSource> = Arc::new(Stuck {
        released: Arc::clone(&released),
    });
    let root = Root::new();
    root.skill("fine", &[("SKILL.md", &instructions("fine", "d"))]);
    let (events, mut subscription) = stream();
    let started = Instant::now();
    let catalog = discover(
        named(vec![
            ("broken", Arc::new(Broken)),
            ("stuck", stuck),
            ("project", root.source()),
        ]),
        &events,
        Duration::from_millis(300),
    )
    .await;
    assert!(started.elapsed() < BOUND, "{:?}", started.elapsed());
    *lock(&released.0) = true;
    released.1.notify_all();
    assert_eq!(catalog.briefs().len(), 1);
    let events = held(&mut subscription);
    let Payload::SkillDiscovered { diagnostics, .. } = &events[0].payload else {
        panic!("{events:?}");
    };
    assert_eq!(
        codes(diagnostics),
        ["source-error - broken", "source-error stuck stuck"]
    );
    assert!(
        diagnostics[0].message.contains("the service is down"),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics[1].message.contains("did not answer"),
        "{diagnostics:?}"
    );
}

/// Every skill is read at once, within the session's places: twenty that never answer cost the
/// start one bound, not twenty, and hold no more than the places' threads.
#[tokio::test]
async fn skills_that_never_answer_cost_the_start_one_bound_and_its_places() {
    struct Hanging {
        inner: Stuck,
        /// How many reads are running, and the most that ran at once.
        reading: Mutex<(usize, usize)>,
    }
    impl SkillSource for Hanging {
        fn skills(&self) -> io::Result<Vec<String>> {
            Ok((0..20).map(|i| format!("s{i}")).collect())
        }
        fn files(&self, _: &str) -> io::Result<Vec<SkillFile>> {
            Ok(vec![SkillFile::new("SKILL.md", "1")])
        }
        fn read(&self, _: &str, _: &str) -> io::Result<Vec<u8>> {
            {
                let mut reading = lock(&self.reading);
                reading.0 += 1;
                reading.1 = reading.1.max(reading.0);
            }
            self.inner.wait();
            lock(&self.reading).0 -= 1;
            Ok(Vec::new())
        }
    }
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let hanging = Arc::new(Hanging {
        inner: Stuck {
            released: Arc::clone(&released),
        },
        reading: Mutex::new((0, 0)),
    });
    let source: Arc<dyn SkillSource> = hanging.clone();
    let started = Instant::now();
    let catalog = discover(
        named(vec![("many", source)]),
        &Events::off(),
        Duration::from_millis(300),
    )
    .await;
    let took = started.elapsed();
    *lock(&released.0) = true;
    released.1.notify_all();
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert!(catalog.briefs().is_empty());
    let most = lock(&hanging.reading).1;
    assert!((1..=SOURCE_OPERATIONS_MAX).contains(&most), "{most}");
}

/// A source's operation runs on a thread of its own, which the runtime does not wait for: one that
/// never returns holds up neither the runtime's shutdown nor, so, the process's exit.
#[test]
fn a_source_that_never_returns_does_not_hold_up_the_runtime() {
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let stuck: Arc<dyn SkillSource> = Arc::new(Stuck {
        released: Arc::clone(&released),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let catalog = runtime.block_on(async {
        let sources = named(vec![("stuck", stuck)]);
        discover(sources, &Events::off(), Duration::from_millis(100)).await
    });
    assert!(catalog.briefs().is_empty());
    let (dropped, shut) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(runtime);
        let _ = dropped.send(());
    });
    let shut = shut.recv_timeout(Duration::from_secs(5));
    *lock(&released.0) = true;
    released.1.notify_all();
    shut.expect("the runtime waited for the source's read to return");
}

/// A diagnostic quotes what its skill holds -- the line a parser stopped at -- with each control
/// character but a line break escaped, so a skill's metadata cannot drive the terminal the
/// diagnostic is shown on.
#[tokio::test]
async fn a_diagnostic_escapes_the_control_characters_it_quotes() {
    let root = Root::new();
    root.skill(
        "noisy",
        &[
            ("SKILL.md", &instructions("noisy", "d")),
            ("skill.py", "# /// script\n# x = \x1b]0;title\x07\n# ///\n"),
        ],
    );
    let (_, _, diagnostics) = discovered(vec![("project", root.source())]).await;
    let [diagnostic] = &diagnostics[..] else {
        panic!("{diagnostics:?}");
    };
    assert_eq!(diagnostic.code, "metadata-invalid");
    let message = &diagnostic.message;
    assert!(
        !message.contains(|c: char| c.is_control() && c != '\n'),
        "{message:?}"
    );
    assert!(message.contains(r"\u{1b}]0;title\u{7}"), "{message:?}");
}

#[tokio::test]
async fn this_repositorys_own_skills_are_discovered_through_the_project_source() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let source: Arc<dyn SkillSource> = Arc::new(DirectorySource::within(&repo, ".agents/skills"));
    let (catalog, _, diagnostics) = discovered(vec![("project", source)]).await;
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    let names: Vec<String> = catalog.briefs().into_iter().map(|b| b.name).collect();
    assert_eq!(names, ["groom-plan", "next-task"]);
}

/// The preamble lists each skill on one line, however its description was written.
#[tokio::test]
async fn a_brief_holds_its_description_on_one_line() {
    let root = Root::new();
    root.skill(
        "a",
        &[(
            "SKILL.md",
            "---\nname: a\ndescription: |\n  One.\n\n  Two\tthree.\n---\n",
        )],
    );
    let (catalog, skills, _) = discovered(vec![("project", root.source())]).await;
    assert_eq!(skills[0].description, "One.\n\nTwo\tthree.");
    assert_eq!(catalog.briefs()[0].description, "One. Two three.");
    assert_eq!(one_line(" a\u{1b}[31mb \n c\r"), "a [31mb c");
}

#[test]
fn a_session_needs_named_distinct_sources() {
    let source = || -> Arc<dyn SkillSource> { Arc::new(Broken) };
    assert!(check_sources(&named(vec![("a", source()), ("b", source())])).is_ok());
    assert!(check_sources(&named(vec![("", source())])).is_err());
    assert!(
        check_sources(&named(vec![("a", source()), ("a", source())]))
            .expect_err("repeated")
            .contains("\"a\"")
    );
}

// ---------------------------------------------------------------------------- the wire

/// A fake interpreter whose host serves `sources`, past the boot line it was told.
async fn wired(sources: Vec<(&str, Arc<dyn SkillSource>)>, bound: Duration) -> (Interpreter, Fake) {
    let (interpreter, mut fake) = Fake::connected().await;
    let catalog = discover(named(sources), &Events::off(), bound).await;
    serve(&catalog, &interpreter, &Events::off());
    if !catalog.skills.is_empty() {
        fake.expect("skills").await;
    }
    (interpreter, fake)
}

/// What the host answers to `request`, sent as the interpreter.
async fn asked(fake: &mut Fake, request: Value) -> Value {
    let id = request["id"].clone();
    fake.send(request).await;
    let answer = fake.expect("skill").await;
    assert_eq!(answer["id"], id, "{answer}");
    answer
}

#[tokio::test]
async fn a_request_to_a_session_with_no_skills_is_answered() {
    let (_interpreter, mut fake) = Fake::connected().await;
    let answer = asked(
        &mut fake,
        json!({"t": "skill.load", "id": 7, "skill": "any"}),
    )
    .await;
    assert_eq!(answer["error"]["code"], "unknown-skill", "{answer}");
}

#[tokio::test]
async fn the_host_answers_for_a_skills_listing_files_and_instructions() {
    let root = Root::new();
    let dir = root.skill(
        "review",
        &[
            ("SKILL.md", &instructions("review", "Reviews.")),
            (
                "skill.py",
                "# /// script\n# requires-python = \">=3.13\"\n# dependencies = [\"a>=1\"]\n\
                 # [tool.outrig.skill]\n# entry = \"run\"\n# bind = { repo = \"git\" }\n\
                 # [tool.outrig.skill.parameters]\n# base = \"The ref.\"\n# ///\n",
            ),
            ("data/ids.txt", "a\nb\n"),
        ],
    );
    let (_interpreter, mut fake) = wired(vec![("project", root.source())], BOUND).await;

    let load = asked(
        &mut fake,
        json!({"t": "skill.load", "id": 1, "skill": "review", "agent": "primary"}),
    )
    .await;
    let ok = &load["ok"];
    assert_eq!(ok["source"], "project", "{load}");
    assert_eq!(ok["files"], json!(["SKILL.md", "data/ids.txt", "skill.py"]));
    assert_eq!(ok["requires_python"], ">=3.13");
    assert_eq!(ok["dependencies"], json!(["a>=1"]));
    assert_eq!(ok["entry"], "run");
    assert_eq!(ok["bind"], json!({"repo": "git"}));
    assert_eq!(ok["parameters"], json!({"base": "The ref."}));
    let digest = ok["digest"].clone();
    let current = asked(
        &mut fake,
        json!({"t": "skill.check", "id": 9, "skill": "review", "digest": digest}),
    )
    .await;
    assert_eq!(current["ok"], json!({}), "{current}");

    let read = asked(
        &mut fake,
        json!({"t": "skill.read", "id": 2, "skill": "review", "path": "data/ids.txt", "digest": digest}),
    )
    .await;
    assert_eq!(read["ok"]["data"], "YQpiCg==", "{read}");
    let body = asked(
        &mut fake,
        json!({"t": "skill.instructions", "id": 3, "skill": "review"}),
    )
    .await;
    assert_eq!(
        body["ok"]["body"], "\n# review\n\nFollow the steps.\n",
        "{body}"
    );

    let unlisted = asked(
        &mut fake,
        json!({"t": "skill.read", "id": 4, "skill": "review", "path": "nope.py", "digest": digest}),
    )
    .await;
    assert_eq!(unlisted["error"]["code"], "source-error", "{unlisted}");
    let unknown = asked(
        &mut fake,
        json!({"t": "skill.load", "id": 5, "skill": "nope"}),
    )
    .await;
    assert_eq!(unknown["error"]["code"], "unknown-skill", "{unknown}");

    fs::write(dir.join("data/ids.txt"), "a\nb\nc\n").expect("changed");
    let changed = asked(
        &mut fake,
        json!({"t": "skill.read", "id": 6, "skill": "review", "path": "skill.py", "digest": digest}),
    )
    .await;
    assert_eq!(changed["error"]["code"], "skill-changed", "{changed}");
    assert!(
        changed["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("reload(\"review\")"),
        "{changed}"
    );
    let checked = asked(
        &mut fake,
        json!({"t": "skill.check", "id": 10, "skill": "review", "digest": digest}),
    )
    .await;
    assert_eq!(checked["error"]["code"], "skill-changed", "{checked}");
}

#[tokio::test]
async fn a_source_past_its_bound_is_answered_as_an_error() {
    let released = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let stuck = Stuck {
        released: Arc::clone(&released),
    };
    // A source that lists and reads its metadata at once, and hangs on any later read.
    struct Later {
        inner: Stuck,
        reads: Mutex<usize>,
    }
    impl SkillSource for Later {
        fn skills(&self) -> io::Result<Vec<String>> {
            Ok(vec!["slow".into()])
        }
        fn files(&self, _: &str) -> io::Result<Vec<SkillFile>> {
            Ok(vec![
                SkillFile::new("SKILL.md", "1"),
                SkillFile::new("data.txt", "1"),
            ])
        }
        fn read(&self, _: &str, path: &str) -> io::Result<Vec<u8>> {
            *lock(&self.reads) += 1;
            if path == "SKILL.md" {
                return Ok(instructions("slow", "d").into_bytes());
            }
            self.inner.wait();
            Ok(Vec::new())
        }
    }
    let source = Arc::new(Later {
        inner: stuck,
        reads: Mutex::new(0),
    });
    let (_interpreter, mut fake) = wired(vec![("slow", source)], Duration::from_millis(300)).await;
    let started = Instant::now();
    let answer = asked(
        &mut fake,
        json!({"t": "skill.read", "id": 1, "skill": "slow", "path": "data.txt"}),
    )
    .await;
    assert_eq!(answer["error"]["code"], "source-error", "{answer}");
    assert!(started.elapsed() < BOUND, "{:?}", started.elapsed());
    *lock(&released.0) = true;
    released.1.notify_all();
}

/// A source of one skill, `moving`, whose listing moves on as `trigger` is read: a file saved
/// while the host reads it.
struct Moving {
    trigger: Mutex<Option<&'static str>>,
    version: Mutex<usize>,
}

impl SkillSource for Moving {
    fn skills(&self) -> io::Result<Vec<String>> {
        Ok(vec!["moving".into()])
    }

    fn files(&self, _: &str) -> io::Result<Vec<SkillFile>> {
        let version = lock(&self.version).to_string();
        Ok(vec![
            SkillFile::new("SKILL.md", version),
            SkillFile::new("data.txt", "1"),
        ])
    }

    fn read(&self, _: &str, path: &str) -> io::Result<Vec<u8>> {
        if *lock(&self.trigger) == Some(path) {
            *lock(&self.version) += 1;
        }
        Ok(match path {
            "SKILL.md" => instructions("moving", "d").into_bytes(),
            _ => b"data".to_vec(),
        })
    }
}

/// What a source answers is served only when its files are, after the read, as they were before
/// it: a file changed meanwhile could be half the old version and half the new.
#[tokio::test]
async fn what_changes_while_it_is_read_is_refused() {
    let moving = Arc::new(Moving {
        trigger: Mutex::new(None),
        version: Mutex::new(0),
    });
    let source: Arc<dyn SkillSource> = moving.clone();
    let (_interpreter, mut fake) = wired(vec![("project", source)], BOUND).await;
    let load = asked(
        &mut fake,
        json!({"t": "skill.load", "id": 1, "skill": "moving"}),
    )
    .await;
    let digest = load["ok"]["digest"].clone();
    assert!(digest.is_string(), "{load}");

    *lock(&moving.trigger) = Some("data.txt");
    let read = asked(
        &mut fake,
        json!({"t": "skill.read", "id": 2, "skill": "moving", "path": "data.txt", "digest": digest}),
    )
    .await;
    assert_eq!(read["error"]["code"], "skill-changed", "{read}");

    // Each request now reads `SKILL.md`, which moves the listing as it is read: the load too,
    // since the files are no longer those whose metadata the host holds.
    *lock(&moving.trigger) = Some("SKILL.md");
    for (id, kind) in [(3, "skill.instructions"), (4, "skill.load")] {
        let answer = asked(&mut fake, json!({"t": kind, "id": id, "skill": "moving"})).await;
        assert_eq!(answer["error"]["code"], "skill-changed", "{answer}");
    }
}

// ---------------------------------------------------------------------------- the loader

/// An interpreter on the host whose session serves `sources`' skills, its primary open, and a
/// subscription to what it publishes.
async fn serving(sources: Vec<(&str, Arc<dyn SkillSource>)>) -> (Interpreter, Subscription) {
    let (events, subscription) = stream();
    let catalog = discover(named(sources), &events, BOUND).await;
    let interpreter = start_on_host_with(events.clone()).await;
    serve(&catalog, &interpreter, &events);
    (interpreter, subscription)
}

/// What `source` printed, run in `interpreter`'s primary.
async fn printed(interpreter: &Interpreter, source: &str) -> String {
    let mut execution = interpreter.submit(&py(source)).expect("submitted");
    match within(execution.outcome()).await {
        Outcome::Ok(report) => report.output,
        other => panic!("{source}: {other:?}"),
    }
}

/// The traceback `source` raised with, run in `interpreter`'s primary, as the agent is shown it.
async fn raised(interpreter: &Interpreter, source: &str) -> String {
    let mut execution = interpreter.submit(&py(source)).expect("submitted");
    match within(execution.outcome()).await {
        Outcome::Error { traceback, .. } => traceback,
        other => panic!("{source}: {other:?}"),
    }
}

/// The skill every loader test imports: `review`, whose entry imports a sibling, and which reads
/// a data file and declares an `@outrig.agent`.
fn review(root: &Root, extra: &[(&str, &str)]) -> PathBuf {
    let skill = py(r#"
        """Reviews a change."""
        from . import sections
        import json
        import outrig

        VALUE = sections.answer()
        STDLIB_JSON = json

        @outrig.agent
        async def summarize(text: str) -> str:
            """Summarize `text`."""
            ...

        def main():
            raise ValueError("boom")
        "#);
    let mut files = vec![
        ("SKILL.md", instructions("review", "Reviews a change.")),
        ("skill.py", skill),
        ("sections.py", "def answer():\n    return 42\n".to_string()),
        ("json.py", "OWN = True\n".to_string()),
        ("data/ids.txt", "a\nb\n".to_string()),
    ];
    files.extend(extra.iter().map(|(path, text)| (*path, text.to_string())));
    let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
    root.skill("review", &files)
}

#[tokio::test]
async fn a_skill_imports_as_a_package_whose_files_arrive_as_they_are_used() {
    let root = Root::new();
    let sentinel = root.path().join("ran");
    review(&root, &[]);
    root.skill(
        "eager",
        &[
            ("SKILL.md", &instructions("eager", "Runs at import.")),
            (
                "skill.py",
                &format!("open({:?}, 'w').close()\n", sentinel.to_string_lossy()),
            ),
        ],
    );
    let counted = Counting::new(root.source());
    let (interpreter, _events) = serving(vec![("project", counted.clone())]).await;
    assert_eq!(counted.count("review", "SKILL.md"), 1);
    assert_eq!(counted.count("review", "skill.py"), 1);

    assert_eq!(
        printed(
            &interpreter,
            "import outrig_skills.eager\nprint(outrig_skills.eager.__outrig_skill__.name)"
        )
        .await,
        "eager\n"
    );
    assert!(!sentinel.exists(), "importing the package ran its skill.py");
    assert_eq!(
        counted.count("eager", "skill.py"),
        1,
        "the package import fetched code"
    );

    assert_eq!(
        printed(&interpreter, "import outrig_skills.review").await,
        ""
    );
    assert_eq!(counted.count("review", "skill.py"), 1);
    assert_eq!(counted.count("review", "sections.py"), 0);
    assert_eq!(
        printed(
            &interpreter,
            r#"
            import json, sys
            import outrig_skills.review.skill as skill
            print(skill.VALUE, skill.STDLIB_JSON is sys.modules["json"], json is sys.modules["json"])
            from outrig_skills.review import json as own
            print(own.OWN, own.__name__)
            "#
        )
        .await,
        "42 True True\nTrue outrig_skills.review.json\n"
    );
    assert_eq!(counted.count("review", "skill.py"), 2);
    assert_eq!(counted.count("review", "sections.py"), 1);
    assert_eq!(counted.count("review", "data/ids.txt"), 0);

    assert_eq!(
        printed(
            &interpreter,
            r#"
            import importlib.resources
            files = importlib.resources.files("outrig_skills.review")
            data = files / "data" / "ids.txt"
            print(sorted(p.name for p in files.iterdir()))
            print(data.is_file(), (files / "absent").is_file(), (files / "data").is_dir())
            "#
        )
        .await,
        "['SKILL.md', 'data', 'json.py', 'sections.py', 'skill.py']\nTrue False True\n"
    );
    assert_eq!(
        counted.count("review", "data/ids.txt"),
        0,
        "listing a file read it"
    );
    assert_eq!(
        printed(
            &interpreter,
            "import importlib.resources\nprint(importlib.resources.files('outrig_skills.review').joinpath('data/ids.txt').read_text().split())"
        )
        .await,
        "['a', 'b']\n"
    );
    assert_eq!(counted.count("review", "data/ids.txt"), 1);
    assert!(!sentinel.exists());
    printed(&interpreter, "import outrig_skills.eager.skill").await;
    assert!(sentinel.exists(), "the entry module ran when imported");
}

#[tokio::test]
async fn a_skill_module_names_its_skill_and_shows_its_source() {
    let root = Root::new();
    review(&root, &[]);
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import hashlib, inspect, traceback, outrig
        import outrig_skills.review
        import outrig_skills.review.skill as skill
        origin = outrig_skills.review.__outrig_skill__
        print(origin.name, origin.source, len(origin.digest))
        print(hasattr(skill, "__file__"), hasattr(outrig_skills.review, "__file__"))
        print(inspect.getsource(skill.main).splitlines()[0])
        try:
            skill.main()
        except ValueError as e:
            frame = traceback.extract_tb(e.__traceback__)[-1]
            print(frame.filename, frame.line)
        source = skill.__loader__.get_source(skill.__name__)
        print(skill.summarize.__outrig_agent__.digest == hashlib.sha256(source.encode()).hexdigest())
        "#,
    )
    .await;
    assert_eq!(
        out,
        "review project 64\nFalse False\ndef main():\n\
         <skill review>/skill.py raise ValueError(\"boom\")\nTrue\n"
    );
}

#[tokio::test]
async fn a_changed_skill_is_refused_and_reload_replaces_exactly_its_own_modules() {
    let root = Root::new();
    let dir = review(&root, &[]);
    root.skill(
        "review-diff",
        &[
            ("SKILL.md", &instructions("review-diff", "Reviews a diff.")),
            ("skill.py", "VALUE = 'diff'\n"),
        ],
    );
    let (interpreter, mut events) = serving(vec![("project", root.source())]).await;
    printed(
        &interpreter,
        "import outrig_skills.review, outrig_skills.review_diff.skill\nold, other = outrig_skills.review, outrig_skills.review_diff",
    )
    .await;
    fs::write(
        dir.join("sections.py"),
        "def answer():\n    return 43  # changed\n",
    )
    .expect("changed");
    assert_eq!(
        printed(
            &interpreter,
            r#"
            import outrig
            try:
                import outrig_skills.review.skill
            except outrig.skills.SkillError as e:
                print(e.code, e.skill)
            "#
        )
        .await,
        "skill-changed review\n"
    );
    assert_eq!(
        printed(
            &interpreter,
            r#"
            import sys
            new = outrig.skills.reload("review")
            import outrig_skills.review.skill
            print(new is not old, outrig_skills.review is new, sys.modules["outrig_skills.review_diff"] is other)
            print(outrig_skills.review.skill.VALUE, old.__outrig_skill__.digest != new.__outrig_skill__.digest)
            "#
        )
        .await,
        "True True True\n43 True\n"
    );
    let imports: Vec<(String, bool, Option<String>)> = held(&mut events)
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::SkillImported {
                skill,
                reload,
                code,
                digest,
                ..
            } => {
                assert!(digest.is_some(), "{event:?}");
                assert_eq!(
                    event.subject.as_ref().map(|s| s.as_str()),
                    Some("agent/primary"),
                    "{event:?}"
                );
                Some((skill.clone(), *reload, code.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        imports,
        [
            ("review".into(), false, None),
            ("review-diff".into(), false, None),
            ("review".into(), true, None),
        ]
    );
}

#[tokio::test]
async fn what_a_skill_needs_is_checked_and_nothing_installed() {
    let site = Root::new();
    let dist = site.path().join("fakepkg-1.2.dist-info");
    fs::create_dir(&dist).expect("a distribution");
    fs::write(
        dist.join("METADATA"),
        "Metadata-Version: 2.1\nName: fakepkg\nVersion: 1.2\n",
    )
    .expect("its metadata");
    let root = Root::new();
    let declaring = |name: &str, block: &str| {
        let block: String = block.lines().map(|line| format!("# {line}\n")).collect();
        root.skill(
            name,
            &[
                ("SKILL.md", &instructions(name, "d")),
                (
                    "skill.py",
                    &format!("# /// script\n{block}# ///\nVALUE = 1\n"),
                ),
            ],
        );
    };
    declaring(
        "met",
        "dependencies = [\"fakepkg>=1.0\", \"absent; python_version < '3'\"]",
    );
    declaring(
        "unmet",
        "dependencies = [\"fakepkg>=2\", \"notinstalled>=1\"]",
    );
    declaring("newer", "requires-python = \">=3.99\"");
    declaring("garbled", "dependencies = [\"not a requirement !!\"]");
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        &format!(
            r#"
            import sys, outrig
            sys.path.append({site:?})
            spawned = []
            sys.addaudithook(lambda event, args: event == "subprocess.Popen" and spawned.append(args))
            import outrig_skills.met
            said = {{}}
            for name in ("unmet", "newer", "garbled"):
                try:
                    __import__(f"outrig_skills.{{name}}")
                except outrig.skills.SkillError as e:
                    said[name] = e
            print(" ".join(said[name].code for name in ("unmet", "newer", "garbled")))
            print(said["unmet"].message)
            print(said["newer"].message)
            print(spawned, any(m.startswith("pip._internal") for m in sys.modules))
            "#,
            site = site.path().to_string_lossy()
        ),
    )
    .await;
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert_eq!(
        lines[0],
        "unsatisfied-dependency requires-python metadata-invalid"
    );
    assert!(lines[1].contains("fakepkg>=2 (1.2 is installed)"), "{out}");
    assert!(
        lines[1].contains("notinstalled>=1 (not installed)"),
        "{out}"
    );
    assert!(lines[1].contains("Nothing was installed"), "{out}");
    assert!(
        lines[2].contains(">=3.99, and this interpreter is 3.13"),
        "{out}"
    );
    assert_eq!(lines[3], "[] False", "{out}");
}

/// What a skill's code raises while it is imported is raised as it was, and the report of one that
/// reaches the agent says when compiled code is why: a file in the skill's listing, a compiled
/// module on `sys.path`, or an `ImportError` the skill raised another exception from.
#[tokio::test]
async fn an_import_failure_is_raised_as_it_was_and_its_report_names_compiled_code() {
    let site = Root::new();
    fs::write(site.path().join("native.abi3.so"), b"not really").expect("a compiled module");
    let root = Root::new();
    let skill = |name: &str, code: &str, extra: &[(&str, &str)]| {
        let mut files = vec![
            ("SKILL.md", instructions(name, "d")),
            ("skill.py", code.to_string()),
        ];
        files.extend(extra.iter().map(|(p, t)| (*p, t.to_string())));
        let files: Vec<(&str, &str)> = files.iter().map(|(p, t)| (*p, t.as_str())).collect();
        root.skill(name, &files);
    };
    skill("raising", "raise ValueError('boom')\n", &[]);
    skill("sibling", "from . import missing\n", &[]);
    skill("undeclared", "import notinstalled_anywhere\n", &[]);
    skill("compiled", "from . import fast\n", &[("fast.abi3.so", "x")]);
    skill(
        "from-compiled",
        "from .fast import go\n",
        &[("fast.cpython-313-x86_64-linux-gnu.so", "x")],
    );
    skill("system", "import native\n", &[]);
    skill(
        "wrapped",
        "try:\n    from . import fast\nexcept ImportError as e:\n    raise RuntimeError('no fast path') from e\n",
        &[("fast.abi3.so", "x")],
    );
    skill(
        "guarded",
        "try:\n    from . import opt\nexcept ImportError:\n    opt = None\n",
        &[("opt.py", "raise ValueError('a real bug')\n")],
    );
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let names = [
        ("raising", false),
        ("sibling", false),
        ("undeclared", false),
        ("compiled", true),
        ("from_compiled", true),
        ("system", true),
        ("wrapped", true),
        ("guarded", false),
    ];
    let out = printed(
        &interpreter,
        &format!(
            r#"
            import sys
            sys.path.append({site:?})
            for name in {names:?}:
                try:
                    __import__(f"outrig_skills.{{name}}.skill")
                except Exception as e:
                    print(name, type(e).__name__)
            "#,
            site = site.path().to_string_lossy(),
            names = names.map(|(name, _)| name),
        ),
    )
    .await;
    assert_eq!(
        out,
        "raising ValueError\nsibling ImportError\nundeclared ModuleNotFoundError\n\
         compiled ImportError\nfrom_compiled ModuleNotFoundError\nsystem ImportError\n\
         wrapped RuntimeError\nguarded ValueError\n"
    );
    for (name, compiled) in names {
        let traceback = raised(&interpreter, &format!("import outrig_skills.{name}.skill")).await;
        assert_eq!(
            traceback.contains("is compiled code"),
            compiled,
            "{name}: {traceback}"
        );
    }
}

#[tokio::test]
async fn a_link_out_of_a_skill_cannot_be_read_by_import_or_as_a_resource() {
    let outside = Root::new();
    fs::write(outside.path().join("secret.txt"), "the host's secret").expect("a host file");
    fs::write(
        outside.path().join("leak.py"),
        "SECRET = 'the host's secret'\n",
    )
    .expect("a module");
    let root = Root::new();
    let dir = root.skill(
        "leaky",
        &[
            ("SKILL.md", &instructions("leaky", "d")),
            ("skill.py", "VALUE = 1\n"),
        ],
    );
    symlink(outside.path().join("secret.txt"), dir.join("secret.txt")).expect("a link");
    symlink(outside.path().join("leak.py"), dir.join("leak.py")).expect("a link");
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import importlib.resources, outrig
        try:
            import outrig_skills.leaky.leak
        except outrig.skills.SkillError as e:
            print(e.code, "symbolic link" in str(e))
        try:
            importlib.resources.files("outrig_skills.leaky").joinpath("secret.txt").read_text()
        except outrig.skills.SkillError as e:
            print(e.code, "symbolic link" in str(e))
        "#,
    )
    .await;
    assert_eq!(out, "source-error True\nsource-error True\n");
}

#[tokio::test]
async fn read_catalog_and_an_unknown_name() {
    let root = Root::new();
    review(&root, &[]);
    root.skill(
        "notes",
        &[("SKILL.md", &instructions("notes", "Takes notes."))],
    );
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import outrig
        print(outrig.skills.read("notes"), end="")
        for name, skill in outrig.skills.catalog().items():
            print(name, skill.source, skill.package, skill.entry)
        try:
            outrig.skills.read("absent")
        except outrig.skills.SkillError as e:
            print(e.code)
        import outrig_skills
        try:
            import outrig_skills.absent
        except ModuleNotFoundError as e:
            print(type(e).__name__)
        "#,
    )
    .await;
    assert_eq!(
        out,
        "\n# notes\n\nFollow the steps.\n\
         notes project outrig_skills.notes None\n\
         review project outrig_skills.review outrig_skills.review.skill\n\
         unknown-skill\nModuleNotFoundError\n"
    );
}

#[tokio::test]
async fn read_after_an_import_is_held_to_the_imported_digest() {
    let root = Root::new();
    let dir = review(&root, &[]);
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    printed(&interpreter, "import outrig_skills.review").await;
    fs::write(
        dir.join("SKILL.md"),
        instructions("review", "Changed and longer."),
    )
    .expect("changed");
    let out = printed(
        &interpreter,
        r#"
        import outrig
        try:
            outrig.skills.read("review")
        except outrig.skills.SkillError as e:
            print(e.code)
        "#,
    )
    .await;
    assert_eq!(out, "skill-changed\n");
}

#[tokio::test]
async fn a_failed_import_is_evented_with_its_code() {
    let root = Root::new();
    root.skill(
        "newer",
        &[
            ("SKILL.md", &instructions("newer", "d")),
            (
                "skill.py",
                "# /// script\n# requires-python = \">=3.99\"\n# ///\n",
            ),
        ],
    );
    let (interpreter, mut events) = serving(vec![("project", root.source())]).await;
    printed(
        &interpreter,
        "try:\n    import outrig_skills.newer\nexcept ImportError:\n    pass",
    )
    .await;
    let imported: Vec<_> = held(&mut events)
        .into_iter()
        .filter(|event| matches!(event.payload, Payload::SkillImported { .. }))
        .collect();
    let [event] = &imported[..] else {
        panic!("{imported:?}");
    };
    let Payload::SkillImported {
        skill,
        source,
        code,
        reason,
        digest,
        ..
    } = &event.payload
    else {
        unreachable!()
    };
    assert_eq!((skill.as_str(), source.as_str()), ("newer", "project"));
    assert_eq!(code.as_deref(), Some("requires-python"));
    assert!(
        reason.as_deref().unwrap_or_default().contains(">=3.99"),
        "{reason:?}"
    );
    assert!(digest.is_some());
}

/// A package whose `__init__.py` fails leaves none of the modules it imported first, so the next
/// import fetches each again at the files' new digest; and both `skill.imported` and the report
/// the agent is shown say compiled code is why, from that import's own listing.
#[tokio::test]
async fn a_failed_package_import_leaves_none_of_its_modules_and_is_labeled_from_its_listing() {
    let root = Root::new();
    let dir = root.skill(
        "retry",
        &[
            ("SKILL.md", &instructions("retry", "d")),
            (
                "__init__.py",
                "from . import part\nif not part.FIXED:\n    raise ValueError('not yet')\n",
            ),
            ("part.py", "FIXED = False\n"),
        ],
    );
    root.skill(
        "initnative",
        &[
            ("SKILL.md", &instructions("initnative", "d")),
            ("__init__.py", "from . import fast\n"),
            ("fast.abi3.so", "x"),
        ],
    );
    let (interpreter, mut events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import sys
        for name in ("retry", "initnative"):
            try:
                __import__(f"outrig_skills.{name}")
            except Exception as e:
                print(name, type(e).__name__)
        print(sorted(key for key in sys.modules if key.startswith("outrig_skills.retry")))
        "#,
    )
    .await;
    assert_eq!(out, "retry ValueError\ninitnative ImportError\n[]\n");
    let codes: Vec<_> = held(&mut events)
        .into_iter()
        .filter_map(|event| match &event.payload {
            Payload::SkillImported { skill, code, .. } => Some((skill.clone(), code.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        codes,
        [
            ("retry".to_string(), Some("import-failed".to_string())),
            (
                "initnative".to_string(),
                Some("native-extension-unavailable".to_string())
            ),
        ]
    );
    let traceback = raised(&interpreter, "import outrig_skills.initnative").await;
    assert!(traceback.contains("is compiled code"), "{traceback}");
    fs::write(dir.join("part.py"), "FIXED = True  # fixed\n").expect("fixed");
    assert_eq!(
        printed(
            &interpreter,
            "import outrig_skills.retry\nprint(outrig_skills.retry.part.FIXED)"
        )
        .await,
        "True\n"
    );
}

/// What a skill's module raises while it is imported is raised as it was, whoever imports it: the
/// skill's own `except` sees it whenever the import runs -- in a function called long after the
/// skill was imported, too -- and so does agent code.
#[tokio::test]
async fn what_a_skills_module_raises_on_import_is_raised_as_it_was() {
    let root = Root::new();
    root.skill(
        "lazy",
        &[
            ("SKILL.md", &instructions("lazy", "d")),
            (
                "skill.py",
                &py(r#"
                    def fallback():
                        try:
                            from . import optional
                        except ModuleNotFoundError as e:
                            return f"fallback for {e.name}"

                    def guarded():
                        try:
                            from . import buggy
                        except ImportError:
                            return "swallowed"
                    "#),
            ),
            ("optional.py", "import notinstalled_anywhere\n"),
            ("buggy.py", "raise ValueError('a real bug')\n"),
        ],
    );
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import outrig_skills.lazy.skill as skill
        print(skill.fallback())
        try:
            skill.guarded()
        except ValueError as e:
            print("raised", e)
        try:
            import outrig_skills.lazy.buggy
        except ValueError as e:
            print("raised", e)
        "#,
    )
    .await;
    assert_eq!(
        out,
        "fallback for notinstalled_anywhere\nraised a real bug\nraised a real bug\n"
    );
}

/// Every way `importlib.resources` reads a file reads a skill's.
#[tokio::test]
async fn importlib_resources_reads_a_skills_file_every_way() {
    let root = Root::new();
    review(&root, &[]);
    let (interpreter, _events) = serving(vec![("project", root.source())]).await;
    let out = printed(
        &interpreter,
        r#"
        import importlib.resources as resources
        package = "outrig_skills.review"
        print(resources.read_text(package, "data/ids.txt").split())
        print(resources.read_text(package, "data", "ids.txt", encoding="utf-8").split())
        with resources.open_text(package, "data/ids.txt") as text:
            print(text.read().split())
        print(resources.read_binary(package, "data/ids.txt"))
        print(resources.is_resource(package, "data/ids.txt"), "ids.txt" in resources.contents(package, "data"))
        data = resources.files(package) / "data" / "ids.txt"
        print(data.read_text().split(), data.read_text(encoding="utf-8", errors="strict").split())
        with resources.as_file(data) as path:
            print(path.read_text().split())
        "#,
    )
    .await;
    assert_eq!(
        out,
        "['a', 'b']\n['a', 'b']\n['a', 'b']\nb'a\\nb\\n'\nTrue True\n['a', 'b'] ['a', 'b']\n\
         ['a', 'b']\n"
    );
}
