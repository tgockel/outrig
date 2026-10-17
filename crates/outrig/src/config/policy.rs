//! `[policy]`: what decides each request agent code makes of a binding.
//!
//! Rules are matched in the bindings process, which holds the objects requests name; this module
//! is their schema, the checks a config is held to, and the tables and version a session sends
//! that process.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The operations a rule may name: every request a binding decides, as `hosted.received` names
/// it. Bookkeeping -- fetching a root, listing a proxy's methods, releasing a reference -- is not
/// decided, so no rule names it, and neither are `buffiter` and `oldslicing`, which the binding
/// refuses before anything is decided.
pub(crate) const OPERATIONS: &[&str] = &[
    "getattr",
    "setattr",
    "delattr",
    "call",
    "callattr",
    "repr",
    "str",
    "cmp",
    "hash",
    "dir",
    "instancecheck",
    "ctxexit",
];

/// What a rule, or `[policy] default`, does with a request it decides. When layers disagree the
/// stricter wins, from least to most strict `allow`, `evaluate`, `escalate`, `deny`: an evaluator
/// may allow without a person, and a person may allow where a deny never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PolicyAction {
    /// The request runs.
    Allow,
    /// The request is refused; its target is never invoked, and no person is asked.
    Deny,
    /// The request waits for the session's escalation handler to answer.
    Escalate,
    /// The request is judged by the evaluator. With none configured, it escalates when a handler
    /// is installed and is denied otherwise; it is never allowed for want of one.
    Evaluate,
}

impl PolicyAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Escalate => "escalate",
            Self::Evaluate => "evaluate",
        }
    }
}

impl std::fmt::Display for PolicyAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One `[[policy.rules]]` entry. A field left out matches anything; `binding`, `type` and
/// `member` match a name exactly, with `*` standing for any run of characters. `type` is the
/// qualified name of the host-side type of the object a request is made on, as `hosted.received`
/// records it as `target`, and a subclass does not match its base's name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct PolicyRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member: Option<String>,
    /// One of the operations `hosted.received` records, exactly. A rule that names none matches a
    /// member's read as well as its call; `call` asks about a call once, with its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    pub action: PolicyAction,
}

impl PolicyRule {
    /// A rule that matches every request, for the setters below to narrow.
    pub fn new(action: PolicyAction) -> Self {
        Self {
            binding: None,
            type_name: None,
            member: None,
            operation: None,
            action,
        }
    }

    pub fn binding(mut self, binding: impl Into<String>) -> Self {
        self.binding = Some(binding.into());
        self
    }

    pub fn type_name(mut self, type_name: impl Into<String>) -> Self {
        self.type_name = Some(type_name.into());
        self
    }

    pub fn member(mut self, member: impl Into<String>) -> Self {
        self.member = Some(member.into());
        self
    }

    pub fn operation(mut self, operation: impl Into<String>) -> Self {
        self.operation = Some(operation.into());
        self
    }
}

/// The `[policy]` table: rules tried in order, the first match deciding, and the action for a
/// request no rule matches.
///
/// In a [`Config`](super::Config) loaded from files this is the operator's -- the global config's
/// -- since a repository's `[policy]` may only add restrictions and is kept apart from it
/// ([`merge`](fn@super::merge)). In a `Config` a program built, it is that program's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
#[non_exhaustive]
pub struct PolicyConfig {
    /// The action for a request no rule matches. In the operator's table, `None` is
    /// [`PolicyAction::Allow`]; in a repository's, a table with no `default` takes no part in a
    /// request none of its rules matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<PolicyAction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<PolicyRule>,
}

impl PolicyConfig {
    /// Whether the table declares nothing.
    pub(crate) fn is_default(&self) -> bool {
        self.default.is_none() && self.rules.is_empty()
    }

    /// The table as the bindings process reads it, `default` resolved when `resolve` is given.
    fn wire(&self, resolve: Option<PolicyAction>) -> Value {
        json!({"default": self.default.or(resolve), "rules": self.rules})
    }

    /// Each action it can reach.
    fn actions(&self) -> impl Iterator<Item = PolicyAction> + '_ {
        self.rules
            .iter()
            .map(|rule| rule.action)
            .chain(self.default)
    }
}

/// The policy in force for a session: the operator's table, each repository table that
/// restricts it, and the version of the two.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    operator: PolicyConfig,
    repo: Vec<PolicyConfig>,
    version: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self::new(PolicyConfig::default(), Vec::new())
    }
}

impl Policy {
    pub(crate) fn new(operator: PolicyConfig, repo: Vec<PolicyConfig>) -> Self {
        let mut policy = Self {
            operator,
            repo,
            version: String::new(),
        };
        let digest = blake3::hash(policy.tables().to_string().as_bytes());
        policy.version = digest.to_hex()[..16].to_string();
        policy
    }

    pub(crate) fn of(config: &super::Config) -> Self {
        Self::new(config.policy.clone(), config.restrictions().to_vec())
    }

    /// Whether any layer declares a rule or a default. A session with none decides nothing, so
    /// its requests carry no decision events.
    pub(crate) fn configured(&self) -> bool {
        !self.operator.is_default() || self.repo.iter().any(|table| !table.is_default())
    }

    /// Whether some request could be held for an answer: an `escalate` anywhere, or an
    /// `evaluate`, which escalates while there is no evaluator.
    pub(crate) fn asks(&self) -> bool {
        let mut actions = self
            .operator
            .actions()
            .chain(self.repo.iter().flat_map(PolicyConfig::actions));
        actions.any(|action| matches!(action, PolicyAction::Escalate | PolicyAction::Evaluate))
    }

    /// The layers, as the bindings process matches them and as the version digests them: an
    /// unset operator default as the `allow` it means, and no empty repository table.
    fn tables(&self) -> Value {
        let repo = self.repo.iter().filter(|table| !table.is_default());
        json!({
            "operator": self.operator.wire(Some(PolicyAction::Allow)),
            "repo": repo.map(|table| table.wire(None)).collect::<Vec<_>>(),
        })
    }

    /// A digest of the rules in force -- each layer's rules in order, and its default -- so a
    /// decision can be traced to the rules that made it.
    pub(crate) fn version(&self) -> &str {
        &self.version
    }

    /// What the bindings process is started with.
    pub(crate) fn wire(&self) -> Value {
        let mut wire = self.tables();
        wire["configured"] = self.configured().into();
        wire
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(member: &str, action: PolicyAction) -> PolicyRule {
        PolicyRule::new(action).binding("repo").member(member)
    }

    fn policy(rules: Vec<PolicyRule>) -> Policy {
        layers(
            PolicyConfig {
                default: None,
                rules,
            },
            Vec::new(),
        )
    }

    fn layers(operator: PolicyConfig, repo: Vec<PolicyConfig>) -> Policy {
        Policy::new(operator, repo)
    }

    #[test]
    fn the_version_names_the_rules_in_force() {
        let push = rule("push", PolicyAction::Escalate);
        let fetch = rule("fetch", PolicyAction::Deny);
        let one = policy(vec![push.clone(), fetch.clone()]);
        assert_eq!(
            one.version(),
            policy(vec![push.clone(), fetch.clone()]).version()
        );
        assert_eq!(one.version().len(), 16);

        let reordered = policy(vec![fetch.clone(), push.clone()]);
        assert_ne!(one.version(), reordered.version(), "the order of two rules");

        let mut changed = push.clone();
        changed.action = PolicyAction::Deny;
        assert_ne!(
            one.version(),
            policy(vec![changed, fetch]).version(),
            "one rule's action"
        );
    }

    #[test]
    fn an_unset_operator_default_is_the_allow_it_means() {
        let default = |action| PolicyConfig {
            default: Some(action),
            rules: Vec::new(),
        };
        let explicit = layers(default(PolicyAction::Allow), Vec::new());
        assert_eq!(policy(Vec::new()).version(), explicit.version());
        assert!(!policy(Vec::new()).configured());
        assert!(explicit.configured());

        let empty = layers(PolicyConfig::default(), vec![PolicyConfig::default()]);
        assert_eq!(
            policy(Vec::new()).version(),
            empty.version(),
            "an empty layer adds no rule"
        );
        let denies = layers(PolicyConfig::default(), vec![default(PolicyAction::Deny)]);
        assert_ne!(policy(Vec::new()).version(), denies.version());
    }

    #[test]
    fn evaluate_and_escalate_ask_and_the_rest_do_not() {
        assert!(!policy(vec![rule("push", PolicyAction::Deny)]).asks());
        assert!(policy(vec![rule("push", PolicyAction::Evaluate)]).asks());
        let escalate = PolicyConfig {
            default: Some(PolicyAction::Escalate),
            rules: Vec::new(),
        };
        assert!(layers(PolicyConfig::default(), vec![escalate]).asks());
    }
}
