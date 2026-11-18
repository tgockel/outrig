//! A session's application code, end to end on the host: read from a directory, an archive or a
//! wheel, copied to its snapshot, its requirement set installed from local wheels, its modules
//! imported as the session starts, and imported from in a kernel. No model is called: the
//! provider is at an address nothing answers. The e2e module at the end runs it in a container.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::agent::mock_http::MODEL;
use crate::config::Config;
use crate::harness::bindings_tests::{output, taken, wheels};
use crate::harness::event::Event;
use crate::harness::{DEFAULT_DRAIN, Session, SessionBuilder, SessionError};
use crate::python::testing::{pip_env, within};
use crate::python::zip::testing::{CENTRAL_HEADER, M, m, zip};

const VAR: &str = "OUTRIG_TEST_APPLICATION_KEY";

/// The application's package: `review.agents` declares an agent class, `review.data` holds a
/// resource.
const FILES: &[(&str, &str)] = &[
    ("review/__init__.py", ""),
    (
        "review/agents.py",
        "import outrig\n\
         \n\
         class Reviewer(outrig.Agent):\n\
         \x20   \"\"\"Review what you are given.\"\"\"\n\
         \n\
         \x20   @outrig.agent\n\
         \x20   async def review(self, path: str) -> str:\n\
         \x20       \"\"\"Review the file at `path`.\"\"\"\n",
    ),
    ("review/data/notes.txt", "notes for the reviewer\n"),
];

/// A repository and a global config beside it, with a provider nothing answers and a `coding`
/// agent; the application's directory; and where its packages and approvals go.
struct Bed {
    _root: tempfile::TempDir,
    repo: PathBuf,
    global: PathBuf,
    app: PathBuf,
    packages: PathBuf,
    approvals: PathBuf,
}

impl Bed {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let path = |name: &str| {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).expect("a directory");
            dir
        };
        let (repo, home, app, packages, approvals) = (
            path("repo"),
            path("home"),
            path("app"),
            path("packages"),
            path("approvals"),
        );
        let global = home.join("config.toml");
        std::fs::write(
            &global,
            format!(
                r#"
default-model = "sonnet"

[providers.claude]
style    = "messages"
base-url = "http://127.0.0.1:9"
api-key  = "${{{VAR}}}"

[models.sonnet]
provider   = "claude"
identifier = "{MODEL}"

[agents.coding]
"#
            ),
        )
        .expect("write the global config");
        let bed = Self {
            _root: root,
            repo,
            global,
            app,
            packages,
            approvals,
        };
        for (path, source) in FILES {
            bed.write(path, source);
        }
        bed
    }

    /// Write `source` to `path` under the application's directory.
    fn write(&self, path: &str, source: &str) {
        let file = self.app.join(path);
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
        std::fs::write(file, source).expect("write");
    }

    /// A session with a primary agent in this repository, with its packages and approvals here.
    fn builder(&self) -> SessionBuilder {
        let config = Config::load(&self.repo, Some(&self.global)).unwrap_or_else(|e| panic!("{e}"));
        SessionBuilder::new(config, Some("coding"), None)
            .secrets(|var: &str| (var == VAR).then(|| "sk-test".to_string()))
            .repo_root(&self.repo)
            .binding_places(&self.packages, pip_env(wheels()), &self.approvals)
    }

    /// The application's directory, importing `review.agents` at start.
    fn spec(&self) -> ApplicationSpec {
        ApplicationSpec::directory(&self.app).imports(["review.agents"])
    }
}

/// `builder` started, and the events it published meanwhile.
async fn start(mut builder: SessionBuilder) -> (Session, Vec<Arc<Event>>) {
    let mut events = builder.subscribe();
    let session = within(builder.start_on_host())
        .await
        .unwrap_or_else(|e| panic!("the session did not start: {e}"));
    (session, taken(&mut events))
}

/// Why `builder` did not start, and the events it published meanwhile.
async fn failed(mut builder: SessionBuilder) -> (String, Vec<Arc<Event>>) {
    let mut events = builder.subscribe();
    match within(builder.start_on_host()).await {
        Ok(_) => panic!("the session started"),
        Err(e) => (e.to_string(), taken(&mut events)),
    }
}

async fn stop(session: Session) {
    let report = within(session.shutdown(DEFAULT_DRAIN)).await;
    assert_eq!(
        report.stopped,
        crate::harness::Stopped::Proven,
        "{report:?}"
    );
}

/// The `application.provisioned` among `events`, which there is exactly one of.
fn provisioned(events: &[Arc<Event>]) -> &Payload {
    let found: Vec<_> = events
        .iter()
        .map(|event| &event.payload)
        .filter(|payload| matches!(payload, Payload::ApplicationProvisioned { .. }))
        .collect();
    assert_eq!(found.len(), 1, "{events:#?}");
    found[0]
}

/// The digest `application.provisioned` carries.
fn digest_of(events: &[Arc<Event>]) -> String {
    match provisioned(events) {
        Payload::ApplicationProvisioned { digest, .. } => digest.clone(),
        _ => unreachable!(),
    }
}

/// Each `application.imported` among `events`, as (module, code).
fn imported(events: &[Arc<Event>]) -> Vec<(String, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            Payload::ApplicationImported { module, code, .. } => {
                Some((module.clone(), code.clone()))
            }
            _ => None,
        })
        .collect()
}

/// A tar of `files`, as an embedder would build one.
fn tar_of(files: &[(&str, &str)]) -> Vec<u8> {
    tar_holding(files).into_inner().expect("a tar")
}

/// A tar of `files` that more members may be appended to.
fn tar_holding(files: &[(&str, &str)]) -> tar::Builder<Vec<u8>> {
    let mut tar = tar::Builder::new(Vec::new());
    for (path, source) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(source.len() as u64);
        header.set_mode(0o644);
        tar.append_data(&mut header, path, source.as_bytes())
            .expect("append");
    }
    tar
}

/// A wheel of the application's package, as a build backend writes one: `FILES`, a `.dist-info`
/// whose `WHEEL` reads `wheel` and whose `RECORD` lists every member, and `more` members after
/// them. Its metadata requires what no session installs.
fn wheel_reading(wheel: &str, more: Vec<M>) -> Vec<u8> {
    let mut members = package();
    members.extend([
        m(
            "review-1.0.dist-info/METADATA",
            "Metadata-Version: 2.1\nName: review\nVersion: 1.0\nRequires-Dist: absent>=1\n",
        ),
        m("review-1.0.dist-info/WHEEL", wheel),
    ]);
    let record = record_of(members.iter().chain(&more));
    members.push(m("review-1.0.dist-info/RECORD", record));
    members.extend(more);
    zip(&members)
}

/// The `RECORD` of a wheel holding `members` and it, as the `csv` module writes one: a path that
/// holds a comma or a quote quoted, and each row ending `\r\n`.
fn record_of<'a>(members: impl IntoIterator<Item = &'a M>) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let mut record = String::new();
    for M { name, data, .. } in members {
        let hash =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(data));
        let path = if name.contains([',', '"']) {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            name.clone()
        };
        record += &format!("{path},sha256={hash},{}\r\n", data.len());
    }
    record + "review-1.0.dist-info/RECORD,,\r\n"
}

/// `FILES`, as a wheel's members.
fn package() -> Vec<M> {
    FILES.iter().map(|(path, source)| m(path, source)).collect()
}

/// [`wheel_reading`] a version 1 `WHEEL` tagged `tag`.
fn wheel_of(tag: &str, more: Vec<M>) -> Vec<u8> {
    let wheel = format!("Wheel-Version: 1.0\nGenerator: test\nRoot-Is-Purelib: true\nTag: {tag}\n");
    wheel_reading(&wheel, more)
}

/// Why the snapshot of the wheel `wheel` was not taken.
fn refusal(wheel: Vec<u8>) -> String {
    refusal_of(&Artifact::Wheel(wheel.into()))
}

/// Why the snapshot of `artifact` was not taken.
fn refusal_of(artifact: &Artifact) -> String {
    let root = tempfile::tempdir().expect("tempdir");
    snapshot_of(artifact, root.path()).unwrap_err().to_string()
}

#[tokio::test]
async fn the_application_imports_in_a_kernel_from_its_snapshot() {
    let bed = Bed::new();
    let (session, events) = start(bed.builder().application(bed.spec())).await;
    let digest = digest_of(&events);
    let Payload::ApplicationProvisioned {
        path,
        mount,
        requires,
        packages,
        imports,
        label,
        ..
    } = provisioned(&events)
    else {
        unreachable!()
    };
    assert_eq!(path.as_deref(), Some(bed.app.as_path()));
    assert!(
        mount.ends_with(&format!("/outrig/applications/{digest}")),
        "{mount}"
    );
    assert!(requires.is_empty() && packages.is_none() && label.is_none());
    assert_eq!(imports, &["review.agents"]);
    assert_eq!(imported(&events), [("review.agents".to_string(), None)]);
    let identity = session.application().expect("the session's application");
    assert_eq!(
        (identity.digest.as_str(), identity.label.as_deref()),
        (digest.as_str(), None)
    );
    assert_eq!(
        output(
            &session,
            &format!(
                "import importlib.resources, outrig, review.agents\n\
                 print(review.agents.__file__.startswith({mount:?} + '/'))\n\
                 print(issubclass(review.agents.Reviewer, outrig.Agent))\n\
                 print((importlib.resources.files('review') / 'data' / 'notes.txt').read_text())"
            )
        )
        .await,
        "True\nTrue\nnotes for the reviewer\n\n"
    );
    stop(session).await;

    // The same directory is the same code; a byte changed is other code.
    let (again, events) = start(bed.builder().application(bed.spec())).await;
    assert_eq!(digest_of(&events), digest);
    stop(again).await;
    bed.write("review/data/notes.txt", "notes for the reviewer!\n");
    let (changed, events) = start(bed.builder().application(bed.spec())).await;
    assert_ne!(digest_of(&events), digest);
    stop(changed).await;
}

#[tokio::test]
async fn an_archive_of_the_same_files_is_the_same_code() {
    let bed = Bed::new();
    let (session, events) = start(bed.builder().application(bed.spec())).await;
    let from_directory = digest_of(&events);
    stop(session).await;

    let spec = ApplicationSpec::archive(tar_of(FILES))
        .imports(["review.agents"])
        .label("review 1.4");
    let (session, events) = start(bed.builder().application(spec)).await;
    let Payload::ApplicationProvisioned {
        path,
        digest,
        label,
        ..
    } = provisioned(&events)
    else {
        unreachable!()
    };
    assert_eq!(
        (path, digest, label.as_deref()),
        (&None, &from_directory, Some("review 1.4"))
    );
    assert_eq!(
        session.application().and_then(|app| app.label.as_deref()),
        Some("review 1.4")
    );
    assert_eq!(
        output(
            &session,
            "import review.agents\nprint(review.agents.Reviewer.__name__)"
        )
        .await,
        "Reviewer\n"
    );
    stop(session).await;
}

/// A wheel's files and its `.dist-info` are the snapshot, whether it is given as a file or as its
/// bytes, and its metadata finds a file installed from `purelib` where it is; the `Requires-Dist`
/// in its metadata installs nothing.
#[tokio::test]
async fn a_wheel_imports_in_a_kernel_with_its_metadata() {
    let bed = Bed::new();
    let file = bed.app.with_file_name("review-1.0-py3-none-any.whl");
    let extra = m(
        "review-1.0.data/purelib/review/data/extra.txt",
        "extra notes\n",
    );
    std::fs::write(&file, wheel_of("py3-none-any", vec![extra])).expect("write the wheel");
    let spec = ApplicationSpec::wheel(&file).imports(["review.agents"]);
    let (session, events) = start(bed.builder().application(spec)).await;
    let digest = digest_of(&events);
    let Payload::ApplicationProvisioned {
        path,
        mount,
        requires,
        packages,
        ..
    } = provisioned(&events)
    else {
        unreachable!()
    };
    assert_eq!(path.as_deref(), Some(file.as_path()));
    assert!(requires.is_empty() && packages.is_none());
    assert_eq!(imported(&events), [("review.agents".to_string(), None)]);
    assert_eq!(
        output(
            &session,
            &format!(
                "import importlib.metadata, importlib.resources, review.agents\n\
                 print(review.agents.__file__.startswith({mount:?} + '/'))\n\
                 print(importlib.metadata.version('review'))\n\
                 print((importlib.resources.files('review') / 'data' / 'notes.txt').read_text())\n\
                 [extra] = [f for f in importlib.metadata.files('review') if f.stem == 'extra']\n\
                 print(extra, extra.read_text())"
            )
        )
        .await,
        "True\n1.0\nnotes for the reviewer\n\nreview/data/extra.txt extra notes\n\n"
    );
    stop(session).await;

    let bytes = std::fs::read(&file).expect("the wheel");
    let spec = ApplicationSpec::wheel_bytes(bytes).imports(["review.agents"]);
    let (session, events) = start(bed.builder().application(spec)).await;
    let Payload::ApplicationProvisioned { path, .. } = provisioned(&events) else {
        unreachable!()
    };
    assert_eq!((path, digest_of(&events)), (&None, digest.clone()));
    stop(session).await;

    // Its `.dist-info` is code the session runs, which the directory of its packages lacks.
    let root = tempfile::tempdir().expect("tempdir");
    let (directory, _) =
        snapshot_of(&Artifact::Directory(bed.app.clone()), root.path()).expect("the snapshot");
    assert_ne!(directory, digest);
}

#[tokio::test]
async fn a_module_that_does_not_import_fails_the_start_before_any_kernel_opens() {
    let bed = Bed::new();
    bed.write("review/base.py", "PRESENT = 1\n");
    bed.write("review/agents.py", "from review.base import ABSENT\n");
    let (error, events) = failed(bed.builder().application(bed.spec())).await;
    assert!(
        error.contains("the application's module `review.agents` did not import (import-failed)"),
        "{error}"
    );
    let last = error.trim_end().lines().last().unwrap_or_default();
    assert!(
        last.starts_with("ImportError: cannot import name 'ABSENT' from 'review.base'"),
        "{error}"
    );
    assert_eq!(
        imported(&events),
        [(
            "review.agents".to_string(),
            Some("import-failed".to_string())
        )]
    );
    // The primary's kernel opens, and its agent starts, only after the application imported.
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, Payload::AgentStarted { .. })),
        "{events:#?}"
    );
}

#[tokio::test]
async fn a_module_that_does_not_import_in_time_fails_the_start_naming_it() {
    let bed = Bed::new();
    bed.write("review/slow.py", "import time\ntime.sleep(60)\n");
    let spec = ApplicationSpec::directory(&bed.app).imports(["review.agents", "review.slow"]);
    let builder = bed
        .builder()
        .application(spec)
        .import_bound(Duration::from_secs(1));
    let (error, events) = failed(builder).await;
    assert_eq!(
        error,
        "the application's module `review.slow` did not import within 1s"
    );
    assert_eq!(imported(&events), [("review.agents".to_string(), None)]);
}

#[tokio::test]
async fn nothing_need_be_imported_at_start() {
    let bed = Bed::new();
    let spec = ApplicationSpec::directory(&bed.app);
    let (session, events) = start(bed.builder().application(spec)).await;
    assert!(imported(&events).is_empty(), "{events:#?}");
    assert_eq!(
        output(
            &session,
            "import review.agents\nprint(review.agents.Reviewer.__name__)"
        )
        .await,
        "Reviewer\n"
    );
    stop(session).await;
}

#[tokio::test]
async fn an_edit_after_the_start_changes_nothing_a_kernel_imports() {
    let bed = Bed::new();
    let (session, _) = start(bed.builder().application(bed.spec())).await;
    bed.write("review/data/notes.txt", "edited\n");
    bed.write("review/late.py", "LATE = True\n");
    assert_eq!(
        output(
            &session,
            "import importlib.resources, importlib.util\n\
             print((importlib.resources.files('review') / 'data' / 'notes.txt').read_text())\n\
             print(importlib.util.find_spec('review.late'))"
        )
        .await,
        "notes for the reviewer\n\nNone\n"
    );
    stop(session).await;
}

#[tokio::test]
async fn the_requirement_set_installs_beside_the_application() {
    let bed = Bed::new();
    let spec = bed.spec().requires(["pure==1.0"]);
    let (session, events) = start(bed.builder().application(spec)).await;
    let Payload::ApplicationProvisioned {
        requires, packages, ..
    } = provisioned(&events)
    else {
        unreachable!()
    };
    let packages = packages.clone().expect("the requirement set's directory");
    assert_eq!(requires, &["pure==1.0"]);
    assert!(
        Path::new(&packages).starts_with(&bed.packages),
        "{packages}"
    );
    assert_eq!(
        output(
            &session,
            &format!("import pure\nprint(pure.ANSWER, pure.__file__.startswith({packages:?}))")
        )
        .await,
        "42 True\n"
    );
    stop(session).await;

    let compiled = bed.spec().requires(["compiled==1.0"]);
    let (error, _) = failed(bed.builder().application(compiled)).await;
    assert!(
        error.contains("the packages the application requires could not be installed")
            && error.contains("only pure-Python wheels are installed"),
        "{error}"
    );
}

#[test]
fn what_cannot_start_fails_the_check() {
    let bed = Bed::new();
    let checked = |builder: SessionBuilder| match builder.check() {
        Ok(()) => panic!("the check passed"),
        Err(SessionError::Start(e)) => e.to_string(),
        Err(e) => panic!("{e}"),
    };
    for bad in [
        "./pure-1.0-py3-none-any.whl",
        "pure @ https://example.com/pure.whl",
    ] {
        let error = checked(bed.builder().application(bed.spec().requires([bad])));
        assert!(
            error.contains("the application's `requires`")
                && error.contains("is not a requirement specifier"),
            "{error}"
        );
    }
    let error = checked(
        bed.builder()
            .application(ApplicationSpec::directory(&bed.app).imports(["review agents"])),
    );
    assert!(error.contains("is not a module's dotted name"), "{error}");
    bed.builder().application(bed.spec()).check().unwrap();
}

/// A directory or an archive holding a compiled module is refused before anything is copied,
/// naming the file; so is a link in either.
#[test]
fn compiled_code_and_links_are_refused_before_anything_is_copied() {
    let bed = Bed::new();
    let root = tempfile::tempdir().expect("tempdir");
    bed.write("review/_speedups.cpython-313-x86_64-linux-gnu.so", "");
    let error = snapshot_of(&Artifact::Directory(bed.app.clone()), root.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(
            "(not-pure-python): review/_speedups.cpython-313-x86_64-linux-gnu.so is a compiled"
        ),
        "{error}"
    );
    let archive = tar_of(&[("review/__init__.py", ""), ("review/native.pyd", "")]);
    let error = snapshot_of(&Artifact::Archive(archive.into()), root.path())
        .unwrap_err()
        .to_string();
    assert!(error.contains("review/native.pyd is a compiled"), "{error}");
    std::fs::remove_file(
        bed.app
            .join("review/_speedups.cpython-313-x86_64-linux-gnu.so"),
    )
    .expect("remove");

    std::os::unix::fs::symlink("agents.py", bed.app.join("review/linked.py")).expect("a link");
    let error = snapshot_of(&Artifact::Directory(bed.app.clone()), root.path())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("\"linked.py\" is a symbolic link, which this source does not follow"),
        "{error}"
    );
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    tar.append_link(&mut header, "review/linked.py", "agents.py")
        .expect("append");
    let error = snapshot_of(
        &Artifact::Archive(tar.into_inner().expect("a tar").into()),
        root.path(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("\"linked.py\" is a symbolic link, which this source does not follow"),
        "{error}"
    );
    let empty: Vec<_> = std::fs::read_dir(root.path()).expect("the root").collect();
    assert!(empty.is_empty(), "{empty:?}");
}

/// What a wheel installs at the top is in the snapshot there: what it holds under `purelib` or
/// `platlib` beside the rest, and the rest of its `.data` where it is. Its `RECORD` names each
/// file where it is. Two members installed at one path are refused.
#[test]
fn a_wheel_is_laid_out_as_an_installer_lays_it_out() {
    let root = tempfile::tempdir().expect("tempdir");
    // Each member, where it is installed, and what it holds.
    let members = [
        (
            "review-1.0.data/purelib/extra.py",
            "extra.py",
            "EXTRA = 1\n",
        ),
        (
            "review-1.0.data/platlib/review/more.py",
            "review/more.py",
            "MORE = 1\n",
        ),
        (
            "review-1.0.data/purelib/review/a,\"b\".txt",
            "review/a,\"b\".txt",
            "quoted\n",
        ),
        (
            "review-1.0.data/scripts/review",
            "review-1.0.data/scripts/review",
            "#!/bin/sh\n",
        ),
    ];
    let more = members
        .iter()
        .map(|(name, _, data)| m(name, data))
        .collect();
    let wheel = wheel_of("py2.py3-none-any", more);
    let (_, dir) = snapshot_of(&Artifact::Wheel(wheel.into()), root.path()).expect("the snapshot");
    for (_, path, _) in members {
        assert!(dir.join(path).is_file(), "{path}");
    }
    for path in ["review-1.0.data/purelib", "review-1.0.data/platlib"] {
        assert!(!dir.join(path).exists(), "{path}");
    }
    let record = std::fs::read_to_string(dir.join("review-1.0.dist-info/RECORD")).expect("RECORD");
    let installed: Vec<M> = members
        .iter()
        .map(|(_, path, data)| m(path, data))
        .collect();
    let rows: Vec<&str> = record.split_inclusive('\n').collect();
    for row in record_of(&installed).split_inclusive('\n') {
        assert!(rows.contains(&row), "{row:?} in {record:?}");
    }
    assert!(
        !record.contains("purelib/") && !record.contains("platlib/"),
        "{record}"
    );

    let wheel = wheel_of(
        "py3-none-any",
        vec![m("review-1.0.data/purelib/review/agents.py", "")],
    );
    let error = refusal(wheel);
    assert!(
        error.contains("two of its members are installed at review/agents.py"),
        "{error}"
    );
}

/// A `RECORD` row is rewritten only where its path moves, quoted as its new path needs and its line
/// ending kept; what cannot be read is kept as it is written.
#[test]
fn a_record_moves_only_the_paths_that_move() {
    let record = "x-1.data/purelib/a.py,sha256=A,1\n\
                  \"x-1.data/platlib/b,c.py\",sha256=B,2\r\n\
                  \"x-1.data/purelib/d.py\",sha256=D,3\n\
                  \"kept.py\",sha256=K,4\n\
                  x-1.data/scripts/s,,\n\
                  x-1.data/purelib/last.py,,";
    assert_eq!(
        installed_record(record),
        "a.py,sha256=A,1\n\
         \"b,c.py\",sha256=B,2\r\n\
         d.py,sha256=D,3\n\
         \"kept.py\",sha256=K,4\n\
         x-1.data/scripts/s,,\n\
         last.py,,"
    );
    let open = "a.py,,\n\"x-1.data/purelib/never closed,,\n";
    assert_eq!(installed_record(open), open);
}

/// A wheel tagged for an ABI or a platform, or holding compiled code under a pure tag, is refused
/// as compiled code is; so are bytes that are no wheel, and a wheel of a version this does not
/// read.
#[test]
fn a_wheel_that_is_compiled_or_no_wheel_is_refused() {
    let error = refusal(wheel_of("cp313-cp313-manylinux_2_17_x86_64", Vec::new()));
    assert!(
        error.contains(
            "(not-pure-python): its wheel review-1.0 is tagged cp313-cp313-manylinux_2_17_x86_64"
        ),
        "{error}"
    );
    let compiled = m("review/_speedups.cpython-313-x86_64-linux-gnu.so", "");
    let error = refusal(wheel_of("py3-none-any", vec![compiled]));
    assert!(
        error.contains("(not-pure-python): review/_speedups.cpython-313-x86_64-linux-gnu.so is"),
        "{error}"
    );
    let error = refusal(wheel_reading(
        "Wheel-Version: 2.0\nTag: py3-none-any\n",
        Vec::new(),
    ));
    assert!(
        error.contains("its Wheel-Version is 2.0, and only a wheel of version 1 is read"),
        "{error}"
    );

    let error = refusal(zip(&package()));
    assert!(error.contains("it holds no `.dist-info`"), "{error}");
    let other = m(
        "other-1.0.dist-info/WHEEL",
        "Wheel-Version: 1.0\nTag: py3-none-any\n",
    );
    let error = refusal(wheel_of("py3-none-any", vec![other]));
    assert!(
        error.contains("2 `.dist-info` directories, other-1.0.dist-info and review-1.0.dist-info"),
        "{error}"
    );
    let error = refusal(b"PK, and then not a zip at all".to_vec());
    assert!(
        error.contains("the application's wheel cannot be read: no end-of-central-directory"),
        "{error}"
    );

    let missing = Path::new("/nonexistent/absent-1.0-py3-none-any.whl");
    let error = refusal_of(&Artifact::WheelFile(missing.to_path_buf()));
    assert!(
        error.contains(&format!(
            "the application's wheel {} cannot be read",
            missing.display()
        )),
        "{error}"
    );
}

/// A wheel member larger than a file may be is refused by the size its zip declares, before any of
/// it is inflated: here a member of one byte declared nine MiB long.
#[test]
fn a_wheel_member_too_large_is_refused_before_it_is_inflated() {
    let mut wheel = wheel_of("py3-none-any", vec![m("review/model.bin", "x")]);
    // The last central directory entry is the member's; its uncompressed size is 24 bytes in.
    let entry = wheel
        .windows(4)
        .rposition(|window| window == CENTRAL_HEADER)
        .expect("a central directory entry");
    wheel[entry + 24..entry + 28].copy_from_slice(&(9u32 << 20).to_le_bytes());
    let error = refusal(wheel);
    assert!(
        error.contains("\"review/model.bin\" is larger than the 8 MiB"),
        "{error}"
    );
}

/// An archive member whose path leads out of the archive is refused.
#[test]
fn an_archive_member_outside_the_archive_is_refused() {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o644);
    // `append_data` refuses `..`, as a builder should; written by hand, as a hostile one would.
    header.set_path("review/__init__.py").expect("a path");
    let name = b"../escape.py";
    header.as_old_mut().name[..name.len()].copy_from_slice(name);
    header.as_old_mut().name[name.len()..].fill(0);
    header.set_cksum();
    tar.append(&header, &b"x"[..]).expect("append");
    let error = snapshot_of(
        &Artifact::Archive(tar.into_inner().expect("a tar").into()),
        tempfile::tempdir().expect("tempdir").path(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("\"../escape.py\" leads out of the archive"),
        "{error}"
    );
}

/// An archive member that reading would skip is never unpacked: a checkout's `.git`, here holding
/// a link to a file the archive lacks, which could not be.
#[test]
fn a_skipped_archive_member_is_never_unpacked() {
    let mut tar = tar_holding(FILES);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Link);
    header.set_size(0);
    tar.append_link(&mut header, ".git/ORIG_HEAD", "absent")
        .expect("append");
    let cache = tempfile::tempdir().expect("tempdir");
    let snapshot = |archive: Vec<u8>| {
        snapshot_of(&Artifact::Archive(archive.into()), cache.path()).expect("the snapshot")
    };
    assert_eq!(
        snapshot(tar.into_inner().expect("a tar")),
        snapshot(tar_of(FILES))
    );
}

/// An archive member larger than a file may be is refused by its header, before any of it is
/// read: here the archive ends where its contents would begin.
#[test]
fn an_archive_member_too_large_is_refused_before_it_is_read() {
    let mut header = tar::Header::new_gnu();
    header.set_path("review/model.bin").expect("a path");
    header.set_size(9 << 20);
    header.set_mode(0o644);
    header.set_cksum();
    let error = snapshot_of(
        &Artifact::Archive(header.as_bytes().to_vec().into()),
        tempfile::tempdir().expect("tempdir").path(),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("\"review/model.bin\" is larger than the 8 MiB"),
        "{error}"
    );
}

/// A member an archive holds more than once is one entry, as the directory it unpacks to holds it
/// once: here a directory's header before each of its files, more headers than a directory may
/// hold entries, and the same code as the directory of those files.
#[test]
fn a_member_given_more_than_once_is_one_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("review")).expect("the package's directory");
    let mut tar = tar::Builder::new(Vec::new());
    for i in 0..600 {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        tar.append_data(&mut header, "review/", &[][..])
            .expect("append");
        let (path, source) = (format!("review/m{i}.py"), format!("N = {i}\n"));
        let mut header = tar::Header::new_gnu();
        header.set_size(source.len() as u64);
        header.set_mode(0o644);
        tar.append_data(&mut header, &path, source.as_bytes())
            .expect("append");
        std::fs::write(dir.path().join(&path), source).expect("write");
    }
    let cache = tempfile::tempdir().expect("tempdir");
    let snapshot = |artifact: Artifact| snapshot_of(&artifact, cache.path()).expect("the snapshot");
    assert_eq!(
        snapshot(Artifact::Archive(tar.into_inner().expect("a tar").into())),
        snapshot(Artifact::Directory(dir.path().to_path_buf()))
    );
}

/// Code whose snapshot is there already is not copied again: the second provisioning writes
/// nothing, and so succeeds in a cache it cannot write to.
#[test]
fn a_snapshot_already_taken_is_not_copied_again() {
    use std::os::unix::fs::PermissionsExt;
    if nix::unistd::geteuid().is_root() {
        // Permissions do not stop root.
        return;
    }
    let bed = Bed::new();
    let root = tempfile::tempdir().expect("tempdir");
    let artifact = Artifact::Directory(bed.app.clone());
    let (digest, dir) = snapshot_of(&artifact, root.path()).expect("the first snapshot");
    assert_eq!(dir, root.path().join(&digest));
    for entry in std::fs::read_dir(root.path()).expect("the root") {
        let path = entry.expect("an entry").path();
        if path != dir {
            std::fs::remove_file(path).expect("remove the lock");
        }
    }
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(root.path(), mode(0o555)).expect("chmod");
    let again = snapshot_of(&artifact, root.path());
    std::fs::set_permissions(root.path(), mode(0o755)).expect("chmod");
    assert_eq!(again.expect("the second snapshot"), (digest, dir));
}

#[cfg(feature = "e2e")]
mod e2e {
    use std::collections::BTreeMap;

    use super::*;
    use crate::LaunchSpec;
    use crate::python::testing::{ALPINE, pull};

    #[tokio::test]
    async fn the_application_imports_in_the_container_from_its_read_only_mount() {
        let bed = Bed::new();
        pull(ALPINE).await;
        let logs = bed.repo.join("logs");
        std::fs::create_dir_all(&logs).expect("a log directory");
        let spec = LaunchSpec::from_image(ALPINE, BTreeMap::new(), logs);
        let builder = bed
            .builder()
            .application(bed.spec().requires(["pure==1.0"]))
            .container(spec);
        let session = tokio::time::timeout(Duration::from_secs(120), builder.start())
            .await
            .expect("the session starts in time")
            .unwrap_or_else(|e| panic!("the session did not start: {e}"));
        let said = output(
            &session,
            "import os, pure, review.agents\n\
             print(review.agents.__file__.startswith('/outrig/app/'))\n\
             print(pure.__file__.startswith('/outrig/bindings/'))\n\
             try:\n\
             \x20   open(os.path.join(os.path.dirname(review.agents.__file__), 'x'), 'w')\n\
             except OSError as e:\n\
             \x20   print('read-only:', type(e).__name__)",
        )
        .await;
        assert_eq!(said, "True\nTrue\nread-only: OSError\n");
        stop(session).await;
    }
}
