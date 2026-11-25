//! Values with host-side `${VAR}` substitutions. Expansion is single-pass;
//! `$${` escapes a literal `${`. API keys retain their whole-value grammar.

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::Deserializer;
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::env_ref::{is_env_var_name, parse_env_ref};

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EnvValueError {
    /// Invalid reference syntax; deliberately does not echo a possibly secret value.
    #[error(
        "invalid env reference at byte {offset}: expected ${{NAME}} (^[A-Z_][A-Z0-9_]*$); use $${{ for a literal opener"
    )]
    InvalidReference { offset: usize },

    #[error("env var {var} is not set")]
    NotPresent { var: String },

    #[error("env var {var} value is not valid UTF-8")]
    NotUnicode { var: String },

    /// A session's [`Secrets`](super::Secrets) had no value for it.
    #[error("${{{var}}} has no value in this session's secrets")]
    Unresolved { var: String },
}

/// A parsed substitution template. Resolved variable values are never stored
/// here, but literal pieces may still be credentials: header callers must
/// hide the source spelling too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvTemplate {
    raw: String,
    parts: Vec<EnvValue>,
}

/// A config value, resolved on the host only where a concrete value is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvValue {
    Literal(String),
    EnvRef(String),
    Template(EnvTemplate),
}

impl EnvValue {
    /// Parse single-pass `${NAME}` substitutions and `$${` escapes.
    /// Malformed reference openers are refused, without echoing the value.
    pub fn from_raw(raw: String) -> Result<Self, EnvValueError> {
        if let Some(var) = parse_env_ref(&raw) {
            return Ok(Self::EnvRef(var.to_string()));
        }
        if !raw.contains("${") {
            return Ok(Self::Literal(raw));
        }
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut offset = 0;
        while offset < raw.len() {
            let tail = &raw[offset..];
            if tail.starts_with("$${") {
                literal.push_str("${");
                offset += 3;
            } else if let Some(tail) = tail.strip_prefix("${") {
                let end = tail
                    .find('}')
                    .ok_or(EnvValueError::InvalidReference { offset })?;
                let var = &tail[..end];
                if !is_env_var_name(var) {
                    return Err(EnvValueError::InvalidReference { offset });
                }
                if !literal.is_empty() {
                    parts.push(Self::Literal(std::mem::take(&mut literal)));
                }
                parts.push(Self::EnvRef(var.to_string()));
                offset += end + 3;
            } else {
                let ch = tail.chars().next().expect("nonempty tail");
                literal.push(ch);
                offset += ch.len_utf8();
            }
        }
        if !literal.is_empty() {
            parts.push(Self::Literal(literal));
        }
        Ok(Self::Template(EnvTemplate { raw, parts }))
    }

    /// All host variable names, in source order (including repetitions).
    pub fn references(&self) -> Vec<&str> {
        match self {
            Self::Literal(_) => Vec::new(),
            Self::EnvRef(var) => vec![var],
            Self::Template(template) => template.parts.iter().flat_map(Self::references).collect(),
        }
    }

    /// Whether the value carries credentials, even among literal text.
    pub fn has_references(&self) -> bool {
        match self {
            Self::Literal(_) => false,
            Self::EnvRef(_) => true,
            Self::Template(template) => template.parts.iter().any(Self::has_references),
        }
    }

    /// Every unlisted variable in this value, reported together.
    pub(crate) fn unlisted_ref(&self, allow: &[String]) -> Option<String> {
        let references = self.references();
        if references.is_empty() {
            return None;
        }
        let allow: HashSet<&str> = allow.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let mut missing = Vec::new();
        for var in references {
            if !allow.contains(var) && seen.insert(var) {
                missing.push(var);
            }
        }
        (!missing.is_empty()).then(|| missing.join(", "))
    }

    /// The config spelling, with literal reference openers escaped. This does
    /// not resolve references, but header callers must still hide its contents.
    pub fn to_raw(&self) -> String {
        match self {
            Self::Literal(s) => s.replace("${", "$${"),
            Self::EnvRef(var) => format!("${{{var}}}"),
            Self::Template(template) => template.raw.clone(),
        }
    }
}

// Error variants retain their `var: String` surface. A template can report a
// comma-separated set of valid names; render each as a distinct reference/list entry.
pub(crate) fn reference_names(vars: &str) -> String {
    vars.split(", ")
        .map(|var| format!("${{{var}}}"))
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn allow_names(vars: &str) -> String {
    vars.split(", ")
        .map(|var| format!("{var:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An [`EnvValue`] resolved against the host environment, kept beside the
/// value it was resolved from.
///
/// The source is what lets a resolved `${VAR}` reach podman or buildah without
/// being shown: a command built from one passes the key by name, with the
/// value in the client's environment rather than on its command line, and
/// every diagnostic spells the entry `KEY=${VAR}` as the config did. A plain
/// `String` carries no such record, so it is passed and shown as written.
///
/// The fields are private so the pair cannot drift: a value edited after
/// resolution would still claim the reference that never produced it.
#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedEnvValue {
    value: String,
    source: EnvValue,
}

impl ResolvedEnvValue {
    /// Resolve `source` once on the host, keeping its original spelling. A missing
    /// or non-Unicode variable produces a typed error naming only the variable.
    pub fn resolve(source: EnvValue) -> Result<Self, EnvValueError> {
        Self::resolve_with(source, &super::EnvSecrets)
    }

    /// Resolve each reference through `secrets` and join the pieces, keeping
    /// the source. Resolved strings are never expanded again. The process
    /// environment is not read unless `secrets` reads it.
    pub fn resolve_with(
        source: EnvValue,
        secrets: &(impl super::Secrets + ?Sized),
    ) -> Result<Self, EnvValueError> {
        fn expand(
            source: &EnvValue,
            secrets: &(impl super::Secrets + ?Sized),
        ) -> Result<String, EnvValueError> {
            match source {
                EnvValue::Literal(s) => Ok(s.clone()),
                EnvValue::EnvRef(var) => secrets.resolve(var).map_err(|err| {
                    let var = var.clone();
                    match err {
                        super::ApiKeyError::NotPresent { .. } => EnvValueError::NotPresent { var },
                        super::ApiKeyError::NotUnicode { .. } => EnvValueError::NotUnicode { var },
                        _ => EnvValueError::Unresolved { var },
                    }
                }),
                EnvValue::Template(template) => {
                    let mut value = String::new();
                    for part in &template.parts {
                        value.push_str(&expand(part, secrets)?);
                    }
                    Ok(value)
                }
            }
        }
        let value = expand(&source, secrets)?;
        Ok(Self { value, source })
    }

    /// `value` as a literal, which is its own source. Nothing is looked up, so
    /// a string shaped like `${VAR}` stays that text, and no value made here
    /// can claim a reference.
    pub fn literal(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            source: EnvValue::Literal(value.clone()),
            value,
        }
    }

    /// The resolved value: the literal, or the referenced variable's value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// The config value this was resolved from.
    pub fn source(&self) -> &EnvValue {
        &self.source
    }

    /// A pair as if `source` had resolved to `value`, without reading the
    /// process environment.
    #[cfg(test)]
    pub(crate) fn assume(source: EnvValue, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            source,
        }
    }
}

/// Only the source: a resolved reference's value is what this type exists to
/// keep out of output.
impl fmt::Debug for ResolvedEnvValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedEnvValue")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Serialize for EnvValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_raw())
    }
}

impl<'de> Deserialize<'de> for EnvValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_raw(raw).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for EnvValue {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "EnvValue".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "description": "Host-side ${NAME} substitutions may appear anywhere; names match ^[A-Z_][A-Z0-9_]*$. Use $${ for a literal ${. Malformed references are refused. Expansion is single-pass, never in the container." })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn a_reference_is_refused_unless_the_list_names_it() {
        let value = EnvValue::from_raw("${TOKEN}".to_string()).unwrap();

        assert_eq!(value.unlisted_ref(&[]), Some("TOKEN".to_string()));
        assert_eq!(
            value.unlisted_ref(&names(&["OTHER"])),
            Some("TOKEN".to_string())
        );
        assert_eq!(value.unlisted_ref(&names(&["OTHER", "TOKEN"])), None);
    }

    /// Both membership checks must scale with repository-controlled templates:
    /// neither a large allowlist nor many distinct missing names should cause
    /// a scan through all previously seen names for every reference.
    #[test]
    fn large_templates_report_unlisted_names_once_in_source_order() {
        const COUNT: usize = 80_000;
        let vars: Vec<String> = (0..COUNT).map(|i| format!("VAR_{i}")).collect();
        let raw = vars
            .iter()
            .map(|var| format!("${{{var}}}"))
            .collect::<String>();
        let value = EnvValue::from_raw(format!("{raw}{raw}")).unwrap();
        assert_eq!(value.unlisted_ref(&[]).unwrap(), vars.join(", "));
        let allow: Vec<String> = vars.iter().step_by(2).cloned().collect();
        let expected = vars
            .iter()
            .skip(1)
            .step_by(2)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        assert_eq!(value.unlisted_ref(&allow).unwrap(), expected);
        assert_eq!(value.unlisted_ref(&vars), None);
    }

    /// Plain text and escaped openers read nothing from the host.
    #[test]
    fn a_literal_names_no_variable() {
        for raw in ["/workspace/.cargo", "$${TOKEN}"] {
            let value = EnvValue::from_raw(raw.to_string()).unwrap();

            assert_eq!(value.unlisted_ref(&[]), None, "{raw:?}");
        }
    }

    /// A reference is what the resolver says, read once per call and never
    /// from the process environment; a literal asks it nothing; a resolver
    /// with no value is an error naming the variable, not its value.
    #[test]
    fn a_reference_resolves_through_the_given_secrets() {
        let asked = std::sync::Mutex::new(Vec::new());
        let secrets = |var: &str| {
            asked.lock().unwrap().push(var.to_string());
            (var == "OUTRIG_TEST_ENV_VALUE_UNSET").then(|| "from-secrets".to_string())
        };

        let resolved = ResolvedEnvValue::resolve_with(
            EnvValue::from_raw("${OUTRIG_TEST_ENV_VALUE_UNSET}".into()).unwrap(),
            &secrets,
        )
        .expect("the resolver has it");
        assert_eq!(resolved.value(), "from-secrets");
        assert_eq!(resolved.source().to_raw(), "${OUTRIG_TEST_ENV_VALUE_UNSET}");
        assert!(!format!("{resolved:?}").contains("from-secrets"));

        let literal =
            ResolvedEnvValue::resolve_with(EnvValue::from_raw("/a/path".into()).unwrap(), &secrets)
                .unwrap();
        assert_eq!(literal.value(), "/a/path");
        assert_eq!(asked.lock().unwrap().len(), 1, "a literal asks nothing");

        let missing = ResolvedEnvValue::resolve_with(
            EnvValue::from_raw("${OTHER}".into()).unwrap(),
            &secrets,
        )
        .expect_err("the resolver has none");
        assert!(matches!(&missing, EnvValueError::Unresolved { var } if var == "OTHER"));
        assert_eq!(
            missing.to_string(),
            "${OTHER} has no value in this session's secrets"
        );
    }
}
