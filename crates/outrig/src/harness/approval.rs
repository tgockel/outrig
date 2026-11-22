//! Whether a binding, or an MCP server dialed at a URL, a repository declared may start.
//!
//! A binding's factory is code the host runs at session start, as the user, before the agent has
//! done anything, and a repository's config is content: a cloned project brings one, and the agent
//! can write it through the workspace mount. So a binding a repository declared starts only once
//! the operator has approved that exact declaration in that repository. The approval is remembered
//! by a digest of the declaration and the repository's root, under the user's state directory, so
//! an unchanged declaration is asked about once and any edit asks again. A binding from the global
//! config or from [`SessionBuilder::bind`](super::SessionBuilder::bind) needs none: the operator
//! or the embedder wrote it.
//!
//! A repository's `url` MCP entry is asked about the same way: the host dials it, from the host's
//! own network, so the entry would otherwise reach loopback and host-only services the session's
//! container cannot -- authority its author chose and the user did not.

use std::io::Write as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Binding;
use crate::outrig_::ConfiguredUrl;
use crate::python::payload::xdg_root;

/// Decides whether a binding, or a `url` MCP entry, a repository's config declared may start, for
/// a session that has not seen that declaration approved before. Called at session start, before
/// anything is installed or dialed, once for each such declaration, on a thread where blocking --
/// on a terminal prompt, say -- is fine.
///
/// Any `Fn(&ApprovalRequest) -> Approval` is one.
pub trait Approver: Send + Sync {
    /// The answer for the declaration `request` describes: [`Approval::Approve`] starts it and
    /// remembers the approval; any other answer starts nothing and remembers nothing.
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

/// A declaration a repository's config made, as the operator is asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ApprovalRequest {
    /// The binding's name, or the MCP server's.
    pub name: String,
    /// The config file that declared it.
    pub declared_in: Option<PathBuf>,
    /// The repository the session runs in, which the approval is for: the same declaration in
    /// another repository is asked about again, since a relative `{ path }` names another
    /// directory there.
    pub repo_root: PathBuf,
    /// The declaration, as the table of TOML that declares it: a binding's whole, a `url` MCP
    /// entry's with its headers by name alone.
    pub declaration: String,
    /// What it declares.
    pub subject: ApprovalSubject,
}

/// What an [`ApprovalRequest`] asks to start.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalSubject {
    /// A binding: its factory runs on this host, as the user, before the agent starts.
    Binding,
    /// An MCP server this host dials, from its own network, the session's agents calling every
    /// tool it offers.
    #[non_exhaustive]
    McpUrl {
        /// Where it is.
        url: String,
        /// The headers each request carries, by name: no value is shown.
        headers: Vec<String>,
    },
}

impl ApprovalSubject {
    /// An MCP server at `url`, each request carrying `headers`, by name: what a session asks
    /// about, built by hand for testing an approver.
    pub fn mcp_url(url: impl Into<String>, headers: impl IntoIterator<Item = String>) -> Self {
        Self::McpUrl {
            url: url.into(),
            headers: headers.into_iter().collect(),
        }
    }
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
            subject: ApprovalSubject::Binding,
        }
    }

    /// The request, about `subject` rather than a binding.
    #[must_use]
    pub fn about(mut self, subject: ApprovalSubject) -> Self {
        self.subject = subject;
        self
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
pub(crate) fn request(name: &str, binding: &Binding, repo_root: &Path) -> Asked {
    let request = ApprovalRequest::new(
        name,
        binding.declared_in(),
        repo_root,
        declaration(name, binding),
    );
    Asked {
        key: digest(
            b"outrig binding approval 1\0",
            &[
                repo_root.as_os_str().as_encoded_bytes(),
                request.declaration.as_bytes(),
            ],
        ),
        request,
    }
}

/// A request, and the key its approval is remembered by.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub(crate) request: ApprovalRequest,
    key: String,
}

/// What the approver is told of the `url` MCP entry `name` in `repo_root`: its table, its URL,
/// its headers by name, and its other keys. Its approval is remembered by all of it, each header's
/// value as declared -- a literal, or a `${VAR}` reference -- included, so changing a value asks
/// again, though no value is shown.
pub(crate) fn mcp_url_request(name: &str, entry: &ConfiguredUrl, repo_root: &Path) -> Asked {
    let spec = &entry.spec;
    let headers: Vec<String> = spec.headers().keys().cloned().collect();
    let declared = toml::Value::try_from(spec).map(|mut declared| {
        if let (toml::Value::Table(table), false) = (&mut declared, headers.is_empty()) {
            table.insert("headers".to_string(), headers.clone().into());
        }
        declared
    });
    let keys = [
        "url",
        "headers",
        "trust-anchor",
        "trust",
        "allow-stateless",
        "call-timeout-secs",
    ];
    let text = shown(&entry.table, declared, &keys);
    let request = ApprovalRequest::new(name, entry.declared_in.clone(), repo_root, text).about(
        ApprovalSubject::mcp_url(spec.url().unwrap_or_default(), headers),
    );
    let declared = serde_json::to_string(spec).unwrap_or_default();
    let key = digest(
        b"outrig mcp url approval 1\0",
        &[
            repo_root.as_os_str().as_encoded_bytes(),
            entry.table.as_bytes(),
            declared.as_bytes(),
        ],
    );
    Asked { request, key }
}

/// Whether `asked` may start: approved before in its repository, or approved now by `approver`,
/// asked on a thread where it may block, the approval then remembered in `store`. Why not, when
/// not.
pub(crate) async fn ask(
    asked: &Asked,
    store: Option<&Store>,
    approver: Option<&Arc<dyn Approver>>,
) -> Result<(), &'static str> {
    if store.is_some_and(|store| store.approved(&asked.key)) {
        return Ok(());
    }
    let Some(approver) = approver else {
        return Err("the session has no approver to ask about its declaration");
    };
    let (approver, request) = (Arc::clone(approver), asked.request.clone());
    let answer = tokio::task::spawn_blocking(move || approver.approve(&request))
        .await
        .unwrap_or(Approval::CannotAsk);
    match answer {
        Approval::Approve => {
            if let Some(Err(e)) = store.map(|store| store.record(&asked.key, &asked.request)) {
                tracing::warn!(
                    target: "outrig::harness",
                    "the approval of `{}` could not be remembered, so the next session asks \
                     again: {e}",
                    asked.request.name
                );
            }
            Ok(())
        }
        Approval::Refuse => Err("the operator did not approve its declaration"),
        _ => Err("nobody could be asked to approve its declaration"),
    }
}

/// `binding` as the `[bindings.<name>]` table that declares it.
fn declaration(name: &str, binding: &Binding) -> String {
    let keys = [
        "description",
        "requires",
        "factory",
        "args",
        "kwargs",
        "serialize",
    ];
    shown(
        &format!("bindings.{name}"),
        toml::Value::try_from(binding),
        &keys,
    )
}

/// `declared` as the table `[table]` that declares it, its `keys` one to a line in the order the
/// reference documents them, every value inline -- `args = [{ path = "." }]` -- as an operator
/// writes one, rather than as a serializer would split nested tables out.
fn shown(
    table: &str,
    declared: Result<toml::Value, impl std::fmt::Display>,
    keys: &[&str],
) -> String {
    let mut text = format!("[{table}]\n");
    match declared {
        Ok(toml::Value::Table(declared)) => {
            for key in keys {
                if let Some(value) = declared.get(*key) {
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

/// What an approval is remembered by, under `tag`: the `parts` -- the repository it was approved
/// in, and exactly what declares it -- each apart from the next. Any edit to a field, and the
/// same declaration in another repository, asks again.
fn digest(tag: &[u8], parts: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tag);
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hasher.update(b"\0");
        }
        hasher.update(part);
    }
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

    /// Whether the declaration remembered by `key` was approved before, in its repository.
    pub(crate) fn approved(&self, key: &str) -> bool {
        self.dir.join(key).is_file()
    }

    /// Remember that `request`'s declaration, by `key`, is approved in its repository. The file
    /// holds what was shown, so the store can be read and pruned by hand; it is written whole or
    /// not at all.
    pub(crate) fn record(&self, key: &str, request: &ApprovalRequest) -> std::io::Result<()> {
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
        file.persist(self.dir.join(key))?;
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
        request(name, binding, Path::new(root)).key
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

    /// A binding's approval is remembered by the digest sessions have always recorded it by, so
    /// one approved before stays approved.
    #[test]
    fn a_bindings_digest_is_the_one_recorded_before() {
        let asked = request("repo", &repo(), Path::new(ROOT));
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"outrig binding approval 1\0");
        hasher.update(ROOT.as_bytes());
        hasher.update(b"\0");
        hasher.update(asked.request.declaration.as_bytes());
        assert_eq!(asked.key, hasher.finalize().to_hex().to_string());
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
        assert!(!store.approved(&asked.key));
        store.record(&asked.key, &asked.request).expect("recorded");
        assert!(store.approved(&asked.key));
        let text = std::fs::read_to_string(dir.path().join("approved").join(&asked.key))
            .expect("the approval's file");
        assert!(text.contains("git:Repo") && text.contains(ROOT), "{text}");
    }
}
