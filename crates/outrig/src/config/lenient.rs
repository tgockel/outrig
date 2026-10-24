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
//! the error's span points at, then try again. An unknown-field error lists
//! every field its table takes, so one error leaves out each key of that
//! table the list lacks. Only this loader is lenient: everything else that
//! deserializes the types -- the JSON schema `get_config_schema` publishes,
//! an `org.outrig.mcp` label, an `image.toml`, a bare `toml::from_str` --
//! still refuses an unknown key.

use std::collections::HashSet;
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
#[derive(Clone, PartialEq)]
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
    let newlines: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
    let warning = |path: &[Seg], offset: usize, kind| ConfigWarning {
        file: file.map(Path::to_path_buf),
        line: newlines.partition_point(|&i| i < offset) + 1,
        key: dotted(path),
        kind,
    };
    // Where each dropped entry starts: its key, or its `[[...]]` header.
    let mut dropped = HashSet::new();
    let mut warnings: Vec<ConfigWarning> = Vec::new();
    // Each error that named an unknown key, with the key's path.
    let mut unknowns = Vec::new();
    // The source an error renders its excerpt from, attached only to the one
    // returned: a copy of the file in every pass's error would grow with the
    // passes.
    let failed = |mut err: toml::de::Error| {
        err.set_input(Some(text));
        err
    };
    loop {
        let pruned = Spanned::new(doc.span(), prune(doc.get_ref(), &dropped));
        let err = match Config::deserialize(ValueDeserializer::from(pruned)) {
            Ok(mut cfg) => {
                warnings.sort_by_key(|w| w.line);
                cfg.warnings = warnings;
                return Ok(cfg);
            }
            Err(err) => err,
        };
        let Some(span) = err.span() else {
            return Err(failed(err).into());
        };
        let unknown = err
            .message()
            .strip_prefix("unknown field `")
            .and_then(|rest| rest.split_once("`, "));
        if let Some((name, fields)) = unknown {
            if follows_dot_in_header(text, span.start) {
                return Err(OutrigError::ConfigDottedKey {
                    source: failed(err),
                });
            }
            let Some((path, table)) =
                holder(&doc, &span, name, &dropped, &[]).filter(|(path, _)| !is_strict(path))
            else {
                return Err(failed(err).into());
            };
            // The message lists every field the table takes, so this one
            // error answers for each key it does not list. `LlmProvider` is
            // internally tagged: its error lands on the table rather than the
            // key, and the list leaves out its tag, `style`. A key after a
            // `.` in a header waits for an error of its own, for the hint.
            let fields: Vec<&str> = fields.split('`').skip(1).step_by(2).collect();
            let tagged = table
                .get_key_value(name)
                .is_some_and(|(key, _)| key.span() != span);
            for (key, _) in table.iter() {
                let k: &str = key.get_ref();
                let start = key.span().start;
                let keep = fields.contains(&k)
                    || (tagged && k == "style")
                    || follows_dot_in_header(text, start);
                if (k == name || !keep) && dropped.insert(start) {
                    let path = child(&path, Seg::Key(k.to_string()));
                    warnings.push(warning(&path, start, ConfigWarningKind::UnknownKey));
                }
            }
            unknowns.push((child(&path, Seg::Key(name.to_string())), err));
            continue;
        }
        let Some((path, offset)) = locate(doc.get_ref(), &span, &dropped, &[]) else {
            return Err(failed(err).into());
        };
        if !skip_bad_values {
            // A strict parse names the unknown key this entry lost, if it lost
            // one: a misspelled `host-pth` rather than the `host-path` it
            // leaves missing.
            let named = unknowns.into_iter().find(|(key, _)| key.starts_with(&path));
            return Err(failed(named.map_or(err, |(_, named)| named)).into());
        }
        if is_strict(&path) {
            return Err(failed(err).into());
        }
        let reason = reason(err.message(), &path, &warnings);
        dropped.insert(offset);
        warnings.push(warning(
            &path,
            offset,
            ConfigWarningKind::InvalidValue { reason },
        ));
    }
}

/// The non-empty string value of a top-level `key` in `text`, for a caller
/// that needs that one key out of a file [`parse`] refused.
///
/// `Ok(None)` is "this file declares nothing this key can be read from", and
/// the caller's default stands. `Err` is "this file cannot be asked": what the
/// parse error cost could have been the key itself, so the error comes back
/// rather than an answer that might be wrong.
///
/// Recovery first, because a file is usually broken somewhere other than the
/// key being asked about -- a half-written `[models.<name>]` block below a
/// good `session-root` -- and the recovered document still carries it.
///
/// What is taken from that document is only a declaration recovery did not
/// have to repair, and that the file states once ([`declares_once`]).
/// Recovery is a best effort at *a* reading of a damaged file, not a reading
/// of what its writer meant, and the two come apart in ways that would
/// otherwise be invisible here:
///
/// - A value with a broken escape is repaired rather than refused: `"/a\uZZ"`
///   recovers as `/a<U+FFFD>ZZ`, a path nobody has.
/// - A string left unterminated swallows the lines below it, so the value
///   carries the rest of the file.
/// - A key declared twice keeps the *first* value, while a file that parsed
///   would have been refused outright -- the question of which one the writer
///   meant is exactly what is unanswerable.
///
/// A table or an empty string under the key is no path either, and is refused
/// the same way.
///
/// Having no answer from the document, what is left is whether the file could
/// be declaring the key somewhere the recovery lost. A document that parsed
/// has no such somewhere, so its silence is the file's own and the default
/// stands. A document that did not is refused unless the text can rule the key
/// out of it ([`mentions_outside`]), because a command acting on the wrong
/// answer -- `outrig clean`, pointed at a session root that holds no records,
/// reads every labeled container as a stray -- would do damage no warning
/// undoes.
///
/// A file that parses cleanly therefore answers `None` whatever it holds under
/// `key`: a wrong *type* in a readable file is the ordinary lenient load's
/// business ([`ConfigWarningKind::InvalidValue`]), not this one's.
pub(super) fn salvage_top_level_string(text: &str, key: &str) -> Result<Option<String>> {
    let (doc, errors) = DeTable::parse_recoverable(text);
    let declared = doc.get_ref().get_key_value(key);
    if let Some((name, value)) = declared
        && let DeValue::String(found) = value.get_ref()
        && !found.is_empty()
        && declares_once(text, key, name.span(), value.span(), &errors)
    {
        return Ok(Some(found.as_ref().to_string()));
    }
    let Some(mut err) = errors.into_iter().next() else {
        return Ok(None);
    };
    // Nothing was recovered under the key, so no part of the text is accounted
    // for: an empty range at the start leaves all of it to answer.
    if declared.is_none() && !mentions_outside(text, key, &(0..0)) {
        return Ok(None);
    }
    err.set_input(Some(text));
    Err(err.into())
}

/// Whether the declaration recovered at `key_span = value_span` is this file's
/// one statement of `key`, stated in text no parse error damaged -- which is
/// the question of whether the value recovered there can be used.
///
/// Two things have to hold. No error may reach the declaration, the whole of
/// it, key and value together, endpoints included, since the errors that
/// matter most here sit exactly at the end of what they damaged (an
/// unterminated string reports at the end of the text it swallowed). And the
/// file must not be declaring the key a second time, which decides nothing on
/// its own -- recovery keeps whichever declaration came first, a property of
/// the parser rather than a statement about what the operator meant, where a
/// file that parsed would have been refused for the duplicate outright.
///
/// A document that parsed is its own witness of that second point: a key
/// declared twice is an error, and it has none. A document that did not parse
/// is not a witness, because recovery can stop short of the end of the file --
/// a bare `default-model` line between two `session-root` lines ends it at the
/// missing `=`, and the second declaration is never visited, leaving an error
/// that points nowhere near either. So the *text* answers instead, and the one
/// reading of it that holds for a region no parser reached is a search for
/// anything that could be a second declaration: the key's own bytes, or a
/// backslash, which in a file that did not parse is a spelling this cannot
/// resolve (`"session-\u0072oot"` is the key and holds none of its bytes).
///
/// That search is deliberately coarse, since the alternative to refusing is a
/// root the operator did not choose: the key named inside some other value, or
/// an escape in one, costs a damaged file its salvage. The declaration's own
/// text is excluded, so the value may hold the key's name or an escape without
/// answering for a second declaration.
fn declares_once(
    text: &str,
    key: &str,
    key_span: Range<usize>,
    value_span: Range<usize>,
    errors: &[toml::de::Error],
) -> bool {
    // A broken `[header` above a top-level key can leave the two spans
    // inverted, so the declaration's extent is their union, not key-then-value.
    let declaration = key_span.start.min(value_span.start)..key_span.end.max(value_span.end);
    !errors.iter().any(|error| reaches(error, &declaration))
        && (errors.is_empty() || !mentions_outside(text, key, &declaration))
}

/// Whether `error`'s span touches `declaration`. An error with no span at all
/// could be about anything, this declaration included.
fn reaches(error: &toml::de::Error, declaration: &Range<usize>) -> bool {
    let Some(span) = error.span() else {
        return true;
    };
    span.start <= declaration.end && declaration.start <= span.end
}

/// Whether the text outside `declaration` could be declaring `key`: it holds
/// the key's bytes, or a backslash, which in a file that did not parse is an
/// escape this cannot resolve. See [`declares_once`] for why a mention is read
/// that broadly.
fn mentions_outside(text: &str, key: &str, declaration: &Range<usize>) -> bool {
    let outside = |(at, _): (usize, &str)| !declaration.contains(&at);
    text.match_indices(key).any(outside) || text.match_indices('\\').any(outside)
}

/// `node` without the entries that start at a `dropped` offset.
fn prune<'i>(node: &DeValue<'i>, dropped: &HashSet<usize>) -> DeValue<'i> {
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

/// The table under `node` that an "unknown field `name`" error at `span` is
/// about, passing over the `dropped` entries, and its path. The table holds
/// `name`, and the error is at that key -- or, for the internally tagged
/// `LlmProvider`, which reads its table as a whole, at the table itself.
fn holder<'a, 'i>(
    node: &'a Spanned<DeValue<'i>>,
    span: &Range<usize>,
    name: &str,
    dropped: &HashSet<usize>,
    path: &[Seg],
) -> Option<(Vec<Seg>, &'a DeTable<'i>)> {
    match node.get_ref() {
        DeValue::Table(table) => {
            // `[providers]` maps names to providers and has no field of its
            // own. A provider written with dotted keys shares its name's span,
            // so an error that lands on the provider would match the name here;
            // the provider answers below instead.
            let providers = matches!(path, [Seg::Key(top)] if top == "providers");
            let holds = !providers
                && table.get_key_value(name).is_some_and(|(key, _)| {
                    !dropped.contains(&key.span().start)
                        && (key.span() == *span || node.span() == *span)
                });
            if holds {
                return Some((path.to_vec(), table));
            }
            table.iter().find_map(|(key, value)| {
                if dropped.contains(&key.span().start) {
                    return None;
                }
                let path = child(path, Seg::Key(key.get_ref().to_string()));
                holder(value, span, name, dropped, &path)
            })
        }
        DeValue::Array(array) => array.iter().enumerate().find_map(|(i, elem)| {
            if dropped.contains(&elem.span().start) {
                return None;
            }
            holder(elem, span, name, dropped, &child(path, Seg::Index(i)))
        }),
        _ => None,
    }
}

/// The entry under `node` that a bad value at `span` costs, passing over the
/// `dropped` ones: its path, with array indices as the file has them, and
/// where it starts. That is the smallest entry holding the value, except that
/// only a list of records gives up one element: any other array goes whole,
/// since an argv short one argument would run something else. The deepest
/// match wins, because an array of tables shares its span with its first
/// table.
fn locate(
    node: &DeValue<'_>,
    span: &Range<usize>,
    dropped: &HashSet<usize>,
    path: &[Seg],
) -> Option<(Vec<Seg>, usize)> {
    match node {
        DeValue::Table(table) => table.iter().find_map(|(key, value)| {
            if dropped.contains(&key.span().start) {
                return None;
            }
            let path = child(path, Seg::Key(key.get_ref().to_string()));
            if let Some(found) = locate(value.get_ref(), span, dropped, &path) {
                return Some(found);
            }
            let bad = value.span() == *span
                || matches!(value.get_ref(), DeValue::Array(array)
                    if array.iter().any(|elem| elem.span().contains(&span.start)));
            bad.then(|| (path, key.span().start))
        }),
        DeValue::Array(array) if is_record_list(path) => {
            array.iter().enumerate().find_map(|(i, elem)| {
                if dropped.contains(&elem.span().start) {
                    return None;
                }
                let path = child(path, Seg::Index(i));
                locate(elem.get_ref(), span, dropped, &path)
                    .or_else(|| (elem.span() == *span).then(|| (path, elem.span().start)))
            })
        }
        _ => None,
    }
}

fn child(path: &[Seg], seg: Seg) -> Vec<Seg> {
    let mut path = path.to_vec();
    path.push(seg);
    path
}

/// `workspace.mounts` and `sidecars.<n>.mounts`: each mount stands alone, so
/// a bad one can go by itself.
fn is_record_list(path: &[Seg]) -> bool {
    match path {
        [Seg::Key(top), Seg::Key(list)] => top == "workspace" && list == "mounts",
        [Seg::Key(top), _, Seg::Key(list)] => top == "sidecars" && list == "mounts",
        _ => false,
    }
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
    use crate::config::McpServerSpec;

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
        // Beside another unknown key the dotted one still gets the hint.
        assert!(matches!(
            parse(
                "[models.opus-4]\n0th = 1\n[models.opus-4.7]\nprovider = \"a\"\n",
                None,
                true
            ),
            Err(OutrigError::ConfigDottedKey { .. })
        ));
    }

    /// An argv or an `args` list short one element would run something else,
    /// so a bad element costs the whole key. A mount stands alone.
    #[test]
    fn only_a_list_of_records_loses_a_single_element() {
        let text = "[images.c]\nimage-name = \"debian\"\n[images.c.mcp]\n\
                    sh = { command = [\"/bin/sh\", \"-c\", { env = \"SCRIPT\" }, \"printf marker\"] }\n\
                    [sidecars.s]\nimage = \"tools\"\nargs = [\"--a\", { x = 1 }, \"--b\"]\n\
                    [[sidecars.s.mounts]]\nhost-path = \"/a\"\n";
        let keys: Vec<_> = warned(text, true).into_iter().map(|w| w.0).collect();
        assert_eq!(
            keys,
            [
                "images.c.mcp.sh.command",
                "sidecars.s.args",
                "sidecars.s.mounts[0]"
            ]
        );
        let cfg = parse(text, None, true).expect("loads");
        assert!(matches!(
            cfg.images["c"].mcp["sh"],
            McpServerSpec::Full { command: None, .. }
        ));
        assert!(cfg.sidecars["s"].args.is_empty());
        assert!(cfg.sidecars["s"].mounts.is_empty());
    }

    /// When a bad value fails the load, and its entry lost an unknown key, the
    /// error is the one a strict parse gives: the misspelling, not the field
    /// it leaves missing.
    #[test]
    fn a_failing_load_names_the_misspelled_key() {
        for (text, key) in [
            (
                "[[workspace.mounts]]\nhost-pth = \"/a\"\ncontainer-path = \"/a\"\n",
                "host-pth",
            ),
            (
                "[providers.p]\nstyle = \"openai\"\nbase-ur = \"u\"\napi-key = \"${KEY}\"\n",
                "base-ur",
            ),
        ] {
            let err = parse(text, None, false).expect_err(text).to_string();
            assert!(err.contains(&format!("unknown field `{key}`")), "{err}");
            let strict = toml::from_str::<Config>(text).expect_err(text);
            assert_eq!(err, strict.to_string());
        }
    }

    /// One error answers for every unknown key in its table, so thousands of
    /// them cost a pass per table rather than one per key.
    #[test]
    fn thousands_of_unknown_keys_load() {
        let mut text = String::from("default-agent = \"coding\"\n");
        for i in 0..5000 {
            text.push_str(&format!("k{i:04} = 1\n"));
        }
        text.push_str("[workspace]\n");
        for i in 0..2000 {
            text.push_str(&format!("k{i:04} = 1\n"));
        }
        let cfg = parse(&text, None, false).expect("loads");
        assert_eq!(cfg.default_agent.as_deref(), Some("coding"));
        assert_eq!(cfg.warnings().len(), 7000);
        assert_eq!(cfg.warnings()[6999].key(), "workspace.k1999");
    }
}
