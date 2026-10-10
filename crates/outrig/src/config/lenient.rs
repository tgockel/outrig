//! Config parsing that skips what it cannot use rather than failing on it.
//!
//! An unknown key is dropped from any config file, so a file written for a
//! newer outrig still loads. The global config also drops a value that fails
//! to deserialize: it is shared by every repo on the machine, and one stale
//! entry should not stop them all. Each drop is a [`ConfigWarning`].
//!
//! Some mistakes are never skipped: a TOML syntax error, a dotted name that
//! wants quoting (that gets its hint), and anything under `[network]` or an
//! `images.<n>.security` / `sidecars.<n>.security` table, where a skipped
//! entry would loosen the sandbox.
//!
//! The types keep `deny_unknown_fields`, so the leniency is a loop around
//! them: deserialize the parsed document, and on failure leave out the entry
//! the error's span points at, then try again. Only this loader is lenient:
//! everything else that deserializes the types -- the JSON schema
//! `get_config_schema` publishes, an `org.outrig.mcp` label, an `image.toml`,
//! a bare `toml::from_str` -- still refuses an unknown key.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml::Spanned;
use toml::de::{DeTable, DeValue, ValueDeserializer};

use super::Config;
use crate::error::{OutrigError, Result};

/// The reason given for a dropped `api-key`, in place of the error's, which
/// quotes the value -- and a literal key is a secret, which a warning would
/// print on every run.
const API_KEY_REASON: &str =
    r#"api-key is not a "${VAR}" reference (VAR must match ^[A-Z_][A-Z0-9_]*$)"#;

/// One entry a config load skipped instead of failing on. See
/// [`Config::warnings`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConfigWarning {
    file: Option<PathBuf>,
    line: usize,
    key: String,
    kind: ConfigWarningKind,
}

/// Why a [`ConfigWarning`]'s entry was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigWarningKind {
    /// A key this outrig does not know.
    UnknownKey,
    /// A value that failed to deserialize. Only the global config skips one.
    #[non_exhaustive]
    InvalidValue { reason: String },
}

impl ConfigWarning {
    /// The config file, or `None` for [`Config::load_from_str`].
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// 1-based line of the key, or of the `[[...]]` header of a skipped
    /// array-of-tables element.
    pub fn line(&self) -> usize {
        self.line
    }

    /// Dotted TOML path, e.g. `workspace.mounts[1].acess`.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn kind(&self) -> &ConfigWarningKind {
        &self.kind
    }
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{}:{}: ", file.display(), self.line)?,
            None => write!(f, "line {}: ", self.line)?,
        }
        match &self.kind {
            ConfigWarningKind::UnknownKey => write!(f, "unknown key `{}`, ignored", self.key),
            ConfigWarningKind::InvalidValue { reason } => {
                write!(f, "`{}` ignored: {reason}", self.key)
            }
        }
    }
}

/// One step down from the document root.
#[derive(Clone)]
enum Seg {
    Key(String),
    Index(usize),
}

/// Parse `text` as a config, skipping each unknown key -- and, with
/// `skip_bad_values`, each value that fails to deserialize -- with a warning
/// naming `file`.
pub(super) fn parse(text: &str, file: Option<&Path>, skip_bad_values: bool) -> Result<Config> {
    let doc = DeTable::parse(text)?;
    let doc = Spanned::new(doc.span(), DeValue::Table(doc.into_inner()));
    // Where each dropped entry starts: its key, or its `[[...]]` header.
    let mut dropped = Vec::new();
    let mut warnings: Vec<ConfigWarning> = Vec::new();
    loop {
        let pruned = Spanned::new(doc.span(), prune(doc.get_ref(), &dropped));
        let mut err = match Config::deserialize(ValueDeserializer::from(pruned)) {
            Ok(mut cfg) => {
                warnings.sort_by_key(|w| w.line);
                cfg.warnings = warnings;
                return Ok(cfg);
            }
            Err(err) => err,
        };
        err.set_input(Some(text));
        let Some(span) = err.span() else {
            return Err(err.into());
        };
        let unknown = err
            .message()
            .strip_prefix("unknown field `")
            .and_then(|rest| rest.split_once("`, "))
            .map(|(name, _)| name);
        if unknown.is_some() && follows_dot_in_header(text, span.start) {
            return Err(OutrigError::ConfigDottedKey { source: err });
        }
        if unknown.is_none() && !skip_bad_values {
            return Err(err.into());
        }
        let Some((path, offset)) = locate(doc.get_ref(), &span, unknown, &dropped, &[])
            .filter(|(path, _)| !is_strict(path))
        else {
            return Err(err.into());
        };
        let kind = match unknown {
            Some(_) => ConfigWarningKind::UnknownKey,
            None => ConfigWarningKind::InvalidValue {
                reason: reason(err.message(), &path, &warnings),
            },
        };
        dropped.push(offset);
        warnings.push(ConfigWarning {
            file: file.map(Path::to_path_buf),
            line: text[..offset].matches('\n').count() + 1,
            key: dotted(&path),
            kind,
        });
    }
}

/// `node` without the entries that start at a `dropped` offset.
fn prune<'i>(node: &DeValue<'i>, dropped: &[usize]) -> DeValue<'i> {
    let keep = |v: &Spanned<DeValue<'i>>| Spanned::new(v.span(), prune(v.get_ref(), dropped));
    match node {
        DeValue::Table(table) => DeValue::Table(
            table
                .iter()
                .filter(|(key, _)| !dropped.contains(&key.span().start))
                .map(|(key, value)| (key.clone(), keep(value)))
                .collect(),
        ),
        DeValue::Array(array) => DeValue::Array(
            array
                .iter()
                .filter(|elem| !dropped.contains(&elem.span().start))
                .map(keep)
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Whether the key at `offset` follows an unquoted `.` inside a `[...]`
/// header: `[models.opus-4.7]` is model `opus-4` with a key `7`, which wants
/// the quoting hint. The first key of a header follows the `[`, and an error
/// about a whole table points at the `[` itself.
fn follows_dot_in_header(text: &str, offset: usize) -> bool {
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..offset];
    before.trim_start().starts_with('[') && before.trim_end().ends_with('.')
}

/// The entry under `node` that an error at `span` is about, passing over the
/// `dropped` ones: its path, with array indices as the file has them, and
/// where it starts. `unknown` is the field an "unknown field" error names;
/// `None` means the value at `span` is bad. The deepest match wins, because an
/// array of tables shares its span with its first table.
fn locate(
    node: &DeValue<'_>,
    span: &Range<usize>,
    unknown: Option<&str>,
    dropped: &[usize],
    path: &[Seg],
) -> Option<(Vec<Seg>, usize)> {
    match node {
        DeValue::Table(table) => table.iter().find_map(|(key, value)| {
            if dropped.contains(&key.span().start) {
                return None;
            }
            let mut path = child(path, Seg::Key(key.get_ref().to_string()));
            if let Some(found) = locate(value.get_ref(), span, unknown, dropped, &path) {
                return Some(found);
            }
            let offset = match (unknown, value.get_ref()) {
                (Some(name), _) if name == key.get_ref() && key.span() == *span => key.span().start,
                // An internally tagged enum reports an unknown field at its
                // table rather than at the key.
                (Some(name), DeValue::Table(inner)) if value.span() == *span => {
                    let (field, _) = inner.get_key_value(name)?;
                    // Gone already, so this is not the error's key; finding it
                    // again would loop.
                    if dropped.contains(&field.span().start) {
                        return None;
                    }
                    path.push(Seg::Key(name.to_string()));
                    field.span().start
                }
                (None, _) if value.span() == *span => key.span().start,
                // A bad element of an array of non-tables costs the array.
                (None, DeValue::Array(array))
                    if array.iter().any(|elem| {
                        !elem.get_ref().is_table() && elem.span().contains(&span.start)
                    }) =>
                {
                    key.span().start
                }
                _ => return None,
            };
            Some((path, offset))
        }),
        DeValue::Array(array) => array.iter().enumerate().find_map(|(i, elem)| {
            if !elem.get_ref().is_table() || dropped.contains(&elem.span().start) {
                return None;
            }
            let path = child(path, Seg::Index(i));
            locate(elem.get_ref(), span, unknown, dropped, &path).or_else(|| {
                (unknown.is_none() && elem.span() == *span).then(|| (path, elem.span().start))
            })
        }),
        _ => None,
    }
}

fn child(path: &[Seg], seg: Seg) -> Vec<Seg> {
    let mut path = path.to_vec();
    path.push(seg);
    path
}

/// `[network]` and the `security` tables decide what a container may reach
/// and do, so nothing in them is skipped.
fn is_strict(path: &[Seg]) -> bool {
    match path {
        [Seg::Key(top), ..] if top == "network" => true,
        [Seg::Key(top), _, Seg::Key(table), ..] => {
            (top == "images" || top == "sidecars") && table == "security"
        }
        _ => false,
    }
}

/// serde's `message`, except where it would quote a secret or blame a missing
/// field this load already dropped. Every `ApiKeyError` an `api-key` can fail
/// to parse with starts `api-key `, and is the one error that quotes its value.
fn reason(message: &str, path: &[Seg], warnings: &[ConfigWarning]) -> String {
    if message.starts_with("api-key ") {
        return API_KEY_REASON.to_string();
    }
    if let Some(field) = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.strip_suffix('`'))
    {
        let dropped = dotted(&child(path, Seg::Key(field.to_string())));
        if warnings.iter().any(|w| w.key == dropped) {
            return format!("its `{field}` was ignored");
        }
    }
    message.to_string()
}

/// `path` as a dotted TOML key, quoting any segment that is not a bare key:
/// `models."opus-4.8".context-window`, `workspace.mounts[1].acess`.
fn dotted(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                let bare = !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
                if bare {
                    out.push_str(key);
                } else {
                    out.push_str(&format!("{key:?}"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each warning as `(key, line, display)`.
    fn warned(text: &str, skip_bad_values: bool) -> Vec<(String, usize, String)> {
        let cfg = parse(text, None, skip_bad_values).expect("loads");
        let warning = |w: &ConfigWarning| (w.key.clone(), w.line, w.to_string());
        cfg.warnings().iter().map(warning).collect()
    }

    #[test]
    fn an_unknown_key_is_skipped_in_any_table() {
        let provider = "[providers.p]\nstyle = \"openai\"\nbase-url = \"u\"\n\
                        api-key = \"${KEY}\"\nrole-alternation = \"strict\"\n";
        assert_eq!(
            warned(provider, false),
            [(
                "providers.p.role-alternation".to_string(),
                5,
                "line 5: unknown key `providers.p.role-alternation`, ignored".to_string(),
            )]
        );
        let mounts = "[[workspace.mounts]]\nhost-path = \"a\"\ncontainer-path = \"/a\"\n\
                      [[workspace.mounts]]\nhost-path = \"b\"\ncontainer-path = \"/b\"\n\
                      acess = \"read-write\"\n";
        assert_eq!(warned(mounts, false)[0].0, "workspace.mounts[1].acess");
        assert_eq!(
            warned("\n[events.sinks]\nfile = \"x\"\n", false)[0].0,
            "events"
        );
    }

    #[test]
    fn a_bad_value_is_skipped_only_when_asked() {
        let text = "[providers.newer]\nstyle = \"openai-responses\"\nbase-url = \"u\"\n\
                    [providers.leaky]\nstyle = \"openai\"\nbase-url = \"u\"\n\
                    api-key = \"sk-literal-key-123\"\n";
        let warnings = warned(text, true);
        let keys: Vec<_> = warnings.iter().map(|w| w.0.as_str()).collect();
        assert_eq!(
            keys,
            [
                "providers.newer",
                "providers.newer.style",
                "providers.leaky"
            ]
        );
        assert!(warnings[0].2.ends_with("ignored: its `style` was ignored"));
        assert!(warnings[2].2.ends_with(API_KEY_REASON));
        assert!(!warnings.iter().any(|w| w.2.contains("sk-literal")));
        assert!(matches!(
            parse(text, None, false),
            Err(OutrigError::Config(_))
        ));
    }

    #[test]
    fn some_mistakes_are_never_skipped() {
        for text in [
            "[network]\nmod = \"filter\"\n",
            "[images.c.security]\ncap-dorp = [\"ALL\"]\n",
            "[providers.openai\n",
        ] {
            assert!(
                matches!(parse(text, None, true), Err(OutrigError::Config(_))),
                "{text}"
            );
        }
        assert!(matches!(
            parse("[models.opus-4.7]\nprovider = \"a\"\n", None, true),
            Err(OutrigError::ConfigDottedKey { .. })
        ));
    }
}
