//! Whether a binding a repository declared may start.
//!
//! A binding's factory is code the host runs at session start, as the user, before the agent has
//! done anything, and a repository's config is content: a cloned project brings one, and the agent
//! can write it through the workspace mount. So a binding a repository declared starts only once
//! the operator has approved that exact declaration in that repository. The approval is remembered
//! by a digest of the declaration and the repository's root, under the user's state directory, so
//! an unchanged declaration is asked about once and any edit asks again. A binding from the global
//! config or from [`SessionBuilder::bind`](super::SessionBuilder::bind) needs none: the operator
//! or the embedder wrote it.

use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use crate::config::Binding;
use crate::python::payload::xdg_root;

/// Decides whether a binding a repository's config declared may start, for a session that has
/// not seen that declaration approved before. Called at session start, before anything is
/// installed, once for each such binding, on a thread where blocking -- on a terminal prompt, say
/// -- is fine.
///
/// Any `Fn(&ApprovalRequest) -> Approval` is one.
pub trait Approver: Send + Sync {
    fn approve(&self, request: &ApprovalRequest) -> Approval;
}

impl<F> Approver for F
where
    F: Fn(&ApprovalRequest) -> Approval + Send + Sync,
{
    fn approve(&self, request: &ApprovalRequest) -> Approval {
        self(request)
    }
}

/// A binding a repository's config declared, as the operator is asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApprovalRequest {
    /// The binding's name.
    pub name: String,
    /// The config file that declared it.
    pub declared_in: Option<PathBuf>,
    /// The repository the session runs in, which the approval is for: the same declaration in
    /// another repository is asked about again, since a relative `{ path }` names another
    /// directory there.
    pub repo_root: PathBuf,
    /// The whole declaration, as a `[bindings.<name>]` table of TOML.
    pub declaration: String,
}

impl ApprovalRequest {
    /// A request about the binding `name`, declared in `declared_in`, for the repository at
    /// `repo_root`, whose declaration reads `declaration`: what a session asks, built by hand for
    /// testing an approver.
    pub fn new(
        name: impl Into<String>,
        declared_in: Option<PathBuf>,
        repo_root: impl Into<PathBuf>,
        declaration: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            declared_in,
            repo_root: repo_root.into(),
            declaration: declaration.into(),
        }
    }
}

/// An approver's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Approval {
    /// Start the binding, and remember the approval: the same declaration in the same repository
    /// starts without asking from now on.
    Approve,
    /// Do not start it. Nothing is remembered: the next session asks again.
    Refuse,
    /// Nobody could be asked -- no terminal, say. The binding does not start, and the next
    /// session asks again.
    CannotAsk,
}

/// What the approver is told: `name`'s declaration in `repo_root`.
pub(crate) fn request(name: &str, binding: &Binding, repo_root: &Path) -> ApprovalRequest {
    ApprovalRequest::new(
        name,
        binding.declared_in(),
        repo_root,
        declaration(name, binding),
    )
}

/// `binding` as the `[bindings.<name>]` table that declares it, one key to a line in the order
/// the reference documents them, every value inline -- `args = [{ path = "." }]` -- as an
/// operator writes one, rather than as a serializer would split nested tables out.
fn declaration(name: &str, binding: &Binding) -> String {
    let mut text = format!("[bindings.{name}]\n");
    match toml::Value::try_from(binding) {
        Ok(toml::Value::Table(table)) => {
            for key in [
                "description",
                "requires",
                "factory",
                "args",
                "kwargs",
                "serialize",
            ] {
                if let Some(value) = table.get(key) {
                    text.push_str(&format!("{key} = {value}\n"));
                }
            }
        }
        Ok(other) => text.push_str(&format!(
            "(it cannot be shown: it is a {})\n",
            other.type_str()
        )),
        Err(e) => text.push_str(&format!("(it cannot be shown: {e})\n")),
    }
    text
}

/// What an approval is remembered by: exactly what the operator was shown -- the declaration,
/// its paths unresolved -- and the repository it was approved in. Any edit to a field, and the
/// same text in another repository, asks again.
fn digest(request: &ApprovalRequest) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"outrig binding approval 1\0");
    hasher.update(request.repo_root.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(request.declaration.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Where approvals are remembered: one file per approved declaration, named by its digest.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store under `dir`.
    pub(crate) fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The user's store: `outrig/approved-bindings/` under `$XDG_STATE_HOME` when that is
    /// absolute, as the XDG spec requires, otherwise under `~/.local/state`. `None` with neither,
    /// and then nothing is remembered.
    pub(crate) fn user() -> Option<Self> {
        xdg_root(
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("HOME"),
            ".local/state",
        )
        .map(|root| Self::at(root.join("outrig/approved-bindings")))
    }

    /// Whether `request`'s declaration was approved before, in its repository.
    pub(crate) fn approved(&self, request: &ApprovalRequest) -> bool {
        self.dir.join(digest(request)).is_file()
    }

    /// Remember that `request`'s declaration is approved in its repository. The file holds what
    /// was approved, so the store can be read and pruned by hand; it is written whole or not at
    /// all.
    pub(crate) fn record(&self, request: &ApprovalRequest) -> std::io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.dir)?;
        writeln!(file, "# approved for {}", request.repo_root.display())?;
        if let Some(declared_in) = &request.declared_in {
            writeln!(file, "# declared in {}", declared_in.display())?;
        }
        file.write_all(request.declaration.as_bytes())?;
        file.persist(self.dir.join(digest(request)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FactoryValue;

    fn repo() -> Binding {
        let mut binding = Binding::new("git:Repo", "The project's repository.");
        binding.requires = vec!["GitPython==3.2.0".into()];
        binding.args = vec![FactoryValue::path(".")];
        binding.serialize = true;
        binding
    }

    const ROOT: &str = "/home/a/project";

    fn digest_of(name: &str, binding: &Binding, root: &str) -> String {
        digest(&request(name, binding, Path::new(root)))
    }

    #[test]
    fn the_digest_changes_with_any_field_and_with_the_repository() {
        let base = digest_of("repo", &repo(), ROOT);
        assert_eq!(base, digest_of("repo", &repo(), ROOT));
        let edits: [fn(&mut Binding); 6] = [
            |b| b.description.push('!'),
            |b| b.requires.push("smmap".into()),
            |b| b.factory = "git:Git".into(),
            |b| b.args = vec![FactoryValue::path("..")],
            |b| {
                b.kwargs
                    .insert("odbt".into(), FactoryValue::String("x".into()));
            },
            |b| b.serialize = false,
        ];
        for edit in edits {
            let mut binding = repo();
            edit(&mut binding);
            assert_ne!(digest_of("repo", &binding, ROOT), base, "{binding:?}");
        }
        assert_ne!(digest_of("other", &repo(), ROOT), base);
        assert_ne!(digest_of("repo", &repo(), "/home/a/fork"), base);
    }

    #[test]
    fn the_declaration_is_shown_as_the_table_that_declares_it() {
        let shown = declaration("repo", &repo());
        assert!(shown.contains("\nargs = [{ path = \".\" }]\n"), "{shown}");
        let parsed: toml::Table = toml::from_str(&shown).expect("the declaration is TOML");
        let back: Binding = parsed["bindings"]["repo"]
            .clone()
            .try_into()
            .expect("the table is a binding");
        assert_eq!(back, repo(), "{shown}");
    }

    #[test]
    fn a_recorded_approval_is_found_again() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("approved"));
        let asked = request("repo", &repo(), Path::new(ROOT));
        assert!(!store.approved(&asked));
        store.record(&asked).expect("recorded");
        assert!(store.approved(&asked));
        let text = std::fs::read_to_string(dir.path().join("approved").join(digest(&asked)))
            .expect("the approval's file");
        assert!(text.contains("git:Repo") && text.contains(ROOT), "{text}");
    }
}
