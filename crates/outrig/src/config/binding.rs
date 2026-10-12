//! `[bindings.<name>]`: an object a session builds on the host, in a process of its own, and
//! presents to its agent under a name (`plan/phase/0003-python/hosted-objects.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::de::Error as _;
use serde::ser::{Error as _, SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::{ConfigSource, resolve_against};

/// One `[bindings.<name>]` entry, or what [`SessionBuilder::bind`] takes: a factory the host
/// calls at session start, in a process of its own, and the object it returns, presented to the
/// agent under the binding's name.
///
/// ```toml
/// [bindings.repo]
/// description = "The project's Git repository, on the host, with your remotes and credentials."
/// requires    = ["GitPython==3.2.0"]
/// factory     = "git:Repo"
/// args        = [{ path = "." }]
/// serialize   = true
/// ```
///
/// A binding declared in a repository's config starts only once the operator approves that exact
/// declaration; one from the global config, or from [`SessionBuilder::bind`], needs no approval.
///
/// [`SessionBuilder::bind`]: crate::harness::SessionBuilder::bind
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct Binding {
    /// Shown to the model beside the name.
    pub description: String,
    /// Requirement specifiers -- a name, extras, a version specifier, markers, never a path or a
    /// URL -- installed as pure-Python wheels into a cache keyed by the set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// The callable that builds the object, as `module:callable`.
    pub factory: String,
    /// Positional arguments to the factory.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<FactoryValue>,
    /// Keyword arguments to the factory.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub kwargs: BTreeMap<String, FactoryValue>,
    /// Whether the library is not thread-safe: the binding then runs one call at a time across
    /// all its connections.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub serialize: bool,
    /// Set at load time, never deserialized. See [`ConfigSource`].
    #[serde(skip)]
    source: Option<ConfigSource>,
}

impl Binding {
    /// A binding that builds its object with `factory`, as `module:callable`, called with no
    /// arguments, and is described to the model as `description`. It requires no packages and is
    /// not serialized.
    pub fn new(factory: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            requires: Vec::new(),
            factory: factory.into(),
            args: Vec::new(),
            kwargs: BTreeMap::new(),
            serialize: false,
            source: None,
        }
    }

    /// The config file this binding was declared in, or `None` for one built in code.
    pub fn config_source(&self) -> Option<&ConfigSource> {
        self.source.as_ref()
    }

    pub(crate) fn set_config_source(&mut self, source: ConfigSource) {
        self.source = Some(source);
    }

    /// The file to name in a diagnostic about this binding, or `None` for one built in code.
    pub(crate) fn declared_in(&self) -> Option<PathBuf> {
        self.source.as_ref().map(ConfigSource::config_path)
    }

    /// Whether this binding came from a repository's config -- a file a cloned project brings
    /// and the agent can write -- rather than from the global config or from code, which the
    /// operator or the embedder wrote. A `--config` file counts as the repository's: it takes
    /// the repository config's place in the merge, and it can sit in the workspace.
    pub(crate) fn repo_declared(&self) -> bool {
        matches!(
            self.source,
            Some(
                ConfigSource::Repo { .. }
                    | ConfigSource::RepoFile { .. }
                    | ConfigSource::Project { .. }
            )
        )
    }

    /// The factory's arguments as the binding process receives them, each `{ path }` made an
    /// absolute host path against `repo_root`.
    pub(crate) fn arguments(&self, repo_root: &Path) -> Result<(Value, Value), String> {
        Ok((
            FactoryValue::array_json(&self.args, repo_root)?,
            FactoryValue::table_json(&self.kwargs, repo_root)?,
        ))
    }
}

/// A literal argument to a binding's factory: what TOML can write, less datetimes, and a path.
///
/// A path is written `{ path = "..." }` and reaches the factory as an absolute host path: OutRig
/// cannot tell which strings are paths -- `"."` might be a directory, a branch or a message -- so
/// the operator says. A relative one resolves against the repository's root, whichever file
/// declared it. Any table holding `path` is such a tag, and holds nothing else.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum FactoryValue {
    String(String),
    Integer(i64),
    /// Finite: a NaN or an infinity has no JSON spelling to cross in.
    Float(f64),
    Boolean(bool),
    Array(Vec<FactoryValue>),
    Table(BTreeMap<String, FactoryValue>),
    /// A host path, as written; `~` and a relative path resolve when the binding starts.
    Path(PathBuf),
}

impl FactoryValue {
    /// A `{ path = ... }` argument.
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    fn to_json(&self, repo_root: &Path) -> Result<Value, String> {
        Ok(match self {
            Self::String(s) => Value::String(s.clone()),
            Self::Integer(i) => Value::from(*i),
            Self::Float(f) => serde_json::Number::from_f64(*f)
                .map(Value::Number)
                .ok_or_else(|| format!("{f} has no JSON spelling to pass to a factory"))?,
            Self::Boolean(b) => Value::Bool(*b),
            Self::Array(items) => Self::array_json(items, repo_root)?,
            Self::Table(table) => Self::table_json(table, repo_root)?,
            Self::Path(path) => {
                // `.` dropped but `..` kept: a host path's `..` is the host's to resolve, and
                // after a symlink it does not undo the component before it.
                let resolved: PathBuf = resolve_against(repo_root, path).components().collect();
                Value::String(
                    resolved
                        .to_str()
                        .ok_or_else(|| format!("the path {} is not UTF-8", resolved.display()))?
                        .to_string(),
                )
            }
        })
    }

    fn array_json(items: &[Self], repo_root: &Path) -> Result<Value, String> {
        items
            .iter()
            .map(|item| item.to_json(repo_root))
            .collect::<Result<_, _>>()
            .map(Value::Array)
    }

    fn table_json(table: &BTreeMap<String, Self>, repo_root: &Path) -> Result<Value, String> {
        table
            .iter()
            .map(|(key, value)| Ok((key.clone(), value.to_json(repo_root)?)))
            .collect::<Result<_, String>>()
            .map(Value::Object)
    }

    /// `value` as an argument, or why it cannot be one.
    fn from_toml(value: toml::Value) -> Result<Self, String> {
        Ok(match value {
            toml::Value::String(s) => Self::String(s),
            toml::Value::Integer(i) => Self::Integer(i),
            toml::Value::Float(f) if f.is_finite() => Self::Float(f),
            toml::Value::Float(f) => {
                return Err(format!(
                    "{f} cannot be passed to a factory: it has no JSON spelling"
                ));
            }
            toml::Value::Boolean(b) => Self::Boolean(b),
            toml::Value::Datetime(d) => {
                return Err(format!(
                    "the datetime {d} cannot be passed to a factory; write it as a string"
                ));
            }
            toml::Value::Array(items) => Self::Array(
                items
                    .into_iter()
                    .map(Self::from_toml)
                    .collect::<Result<_, _>>()?,
            ),
            toml::Value::Table(mut table) => match table.remove("path") {
                Some(toml::Value::String(path)) if table.is_empty() => Self::Path(path.into()),
                Some(toml::Value::String(_)) => {
                    let other: Vec<_> = table.keys().map(String::as_str).collect();
                    return Err(format!(
                        "a `{{ path = ... }}` argument holds `path` and nothing else; this one \
                         also holds {}",
                        other.join(", ")
                    ));
                }
                Some(other) => {
                    return Err(format!(
                        "`{{ path = ... }}` takes a string; got a {}",
                        other.type_str()
                    ));
                }
                None => Self::Table(
                    table
                        .into_iter()
                        .map(|(key, value)| Ok((key, Self::from_toml(value)?)))
                        .collect::<Result<_, String>>()?,
                ),
            },
        })
    }
}

impl<'de> Deserialize<'de> for FactoryValue {
    // Through `toml::Value`: a hand-written visitor would be handed a datetime as a one-key
    // table, and take it for one.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_toml(toml::Value::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

impl Serialize for FactoryValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::String(s) => serializer.serialize_str(s),
            Self::Integer(i) => serializer.serialize_i64(*i),
            Self::Float(f) => serializer.serialize_f64(*f),
            Self::Boolean(b) => serializer.serialize_bool(*b),
            Self::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Self::Table(table) => {
                let mut map = serializer.serialize_map(Some(table.len()))?;
                for (key, value) in table {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
            Self::Path(path) => {
                let text = path
                    .to_str()
                    .ok_or_else(|| S::Error::custom("a binding path that is not UTF-8"))?;
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("path", text)?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_keeps_its_parent_components_for_the_host_to_resolve() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("left")).expect("left");
        std::fs::create_dir_all(root.join("right/child")).expect("right/child");
        std::fs::write(root.join("right/data.txt"), "right").expect("right's data");
        std::fs::write(root.join("left/data.txt"), "left").expect("left's data");
        std::os::unix::fs::symlink(root.join("right/child"), root.join("left/link"))
            .expect("the link");
        let mut binding = Binding::new("m:f", "d");
        binding.args = vec![
            FactoryValue::path("left/link/../data.txt"),
            FactoryValue::path("."),
        ];
        let (args, _) = binding.arguments(root).expect("the arguments");
        let named = args[0].as_str().expect("a string");
        assert_eq!(
            std::fs::read_to_string(named).expect("the file it names"),
            "right",
            "{named}"
        );
        assert_eq!(args[1].as_str(), root.to_str(), "`.` is the root itself");
    }
}
