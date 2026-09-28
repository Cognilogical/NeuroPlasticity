use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// What kind of rule this is.
///
/// The distinction is the difference between "auto-patching" and "auto-patching
/// the safe parts": a sandboxed behavioral tweak and a safety/compliance
/// constraint are not the same kind of data (F2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuleClass {
    /// Optimizer may add, edit, or drop freely.
    #[default]
    Behavioral,
    /// Immutable to the optimizer. Any change requires human sign-off.
    Constraint,
}

/// One rule in `target_rules_file`.
///
/// Deserializes from either a bare string (a `behavioral` rule, which is what
/// every existing rules file contains) or an object, so manifests and rules
/// files keep working unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Rule {
    Text(String),
    Classified {
        #[serde(default)]
        class: RuleClass,
        text: String,
    },
}

impl Rule {
    pub fn behavioral(text: impl Into<String>) -> Self {
        Rule::Text(text.into())
    }

    pub fn constraint(text: impl Into<String>) -> Self {
        Rule::Classified {
            class: RuleClass::Constraint,
            text: text.into(),
        }
    }

    pub fn class(&self) -> RuleClass {
        match self {
            Rule::Text(_) => RuleClass::Behavioral,
            Rule::Classified { class, .. } => *class,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Rule::Text(text) => text,
            Rule::Classified { text, .. } => text,
        }
    }

    /// Serialize back to the shape the author wrote, so a rules file that used
    /// bare strings stays bare strings.
    pub fn to_storage(&self) -> serde_json::Value {
        match self {
            Rule::Text(text) => serde_json::Value::String(text.clone()),
            Rule::Classified { class, text } => serde_json::json!({
                "class": class,
                "text": text,
            }),
        }
    }
}

/// How protected rules are identified, since rule text drifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProtectedMatch {
    #[default]
    Exact,
    Prefix,
}

/// Manifest-level policy describing which rules the optimizer may not touch.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RulePolicy {
    /// Class applied to rules with no explicit class. `behavioral` by default.
    #[serde(default)]
    pub default_class: RuleClass,
    /// Rules the optimizer may never modify or drop.
    #[serde(default)]
    pub protected: Vec<String>,
    #[serde(default)]
    pub protected_match: ProtectedMatch,
}

impl RulePolicy {
    /// Whether a rule is protected, per `protected_match`.
    pub fn protects(&self, rule: &Rule) -> bool {
        let text = rule.text().trim();
        self.protected.iter().any(|pattern| {
            let pattern = pattern.trim();
            match self.protected_match {
                ProtectedMatch::Exact => text == pattern,
                ProtectedMatch::Prefix => text.starts_with(pattern),
            }
        })
    }

    /// The rules this policy forbids changing.
    pub fn protected_rules<'a>(&self, rules: &'a [Rule]) -> Vec<&'a Rule> {
        rules.iter().filter(|r| self.protects(r)).collect()
    }
}

/// A change to a protected rule that the optimizer was not allowed to make.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantinedChange {
    pub rule_text: String,
    pub reason: String,
}

/// The outcome of reconciling an optimizer-proposed rule set against policy.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Quarantine {
    /// Protected rules that were dropped or altered by the optimizer.
    pub changes: Vec<QuarantinedChange>,
    /// The rules file to actually persist, with protected rules restored.
    pub accepted: Vec<Rule>,
}

impl Quarantine {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// Reconcile the optimizer's proposed rules against `policy`.
///
/// Any protected rule missing from `proposed`, or whose text has changed, is
/// restored to its original value and recorded for human review. The optimizer
/// cannot trade a constraint for a test.
pub fn enforce_policy(original: &[Rule], proposed: &[Rule], policy: &RulePolicy) -> Quarantine {
    let mut accepted: Vec<Rule> = proposed
        .iter()
        .filter(|rule| !policy.protects(rule) || policy_is_unchanged(original, rule))
        .cloned()
        .collect();

    let mut changes = Vec::new();

    for original_rule in policy.protected_rules(original) {
        let still_present = accepted
            .iter()
            .any(|rule| rule.text().trim() == original_rule.text().trim());

        if !still_present {
            changes.push(QuarantinedChange {
                rule_text: original_rule.text().to_string(),
                reason: "protected rule was removed or altered by the optimizer".to_string(),
            });
            accepted.push(original_rule.clone());
        }
    }

    // Preserve the original ordering of protected rules so the file stays
    // diffable, and keep new behavioral rules in the optimizer's order.
    let mut ordered: Vec<Rule> = Vec::with_capacity(accepted.len());
    for original_rule in original {
        if let Some(pos) = accepted
            .iter()
            .position(|r| r.text().trim() == original_rule.text().trim())
        {
            ordered.push(accepted.remove(pos));
        }
    }
    ordered.extend(accepted.into_iter());

    Quarantine {
        changes,
        accepted: ordered,
    }
}

fn policy_is_unchanged(original: &[Rule], proposed: &Rule) -> bool {
    original
        .iter()
        .any(|o| o.text().trim() == proposed.text().trim() && o.class() == proposed.class())
}

/// Read a rules file, accepting both bare strings and classified objects.
pub fn parse_rules(content: &str) -> Result<Vec<Rule>> {
    let value: serde_json::Value = serde_json::from_str(content)
        .map_err(|e| anyhow::anyhow!("rules file is not valid JSON: {}", e))?;

    match value {
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| {
                serde_json::from_value::<Rule>(item.clone())
                    .map_err(|e| anyhow::anyhow!("invalid rule entry {}: {}", item, e))
            })
            .collect(),
        // A bare string is a single rule, not a rules file.
        serde_json::Value::String(text) => Ok(vec![Rule::Text(text)]),
        other => bail!("rules file must be a JSON array of rules, got {}", other),
    }
}

/// Serialize a rule set, preserving bare strings where the input had them.
pub fn serialize_rules(rules: &[Rule]) -> Result<String> {
    let values: Vec<serde_json::Value> = rules.iter().map(|r| r.to_storage()).collect();
    Ok(serde_json::to_string_pretty(&values)?)
}

/// Plain texts, for prompts and duplicate checks.
pub fn rule_texts(rules: &[Rule]) -> Vec<String> {
    rules.iter().map(|r| r.text().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with(patterns: &[&str], m: ProtectedMatch) -> RulePolicy {
        RulePolicy {
            default_class: RuleClass::Behavioral,
            protected: patterns.iter().map(|s| s.to_string()).collect(),
            protected_match: m,
        }
    }

    // --- Backward compatibility ---

    #[test]
    fn bare_strings_parse_as_behavioral_rules() {
        let rules = parse_rules(r#"["Always answer concisely.", "Cite sources."]"#).unwrap();
        assert_eq!(rules.len(), 2);
        assert!(rules.iter().all(|r| r.class() == RuleClass::Behavioral));
        assert_eq!(rules[0].text(), "Always answer concisely.");
    }

    #[test]
    fn bare_strings_round_trip_unchanged() {
        let original = r#"["one", "two"]"#;
        let rules = parse_rules(original).unwrap();
        let out = serialize_rules(&rules).unwrap();
        let reparsed: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        assert_eq!(reparsed[0], serde_json::Value::String("one".into()));
        assert_eq!(reparsed[1], serde_json::Value::String("two".into()));
    }

    #[test]
    fn classified_objects_parse() {
        let rules = parse_rules(
            r#"[{"class":"constraint","text":"Escalate emergencies to 911 immediately"},
                {"class":"behavioral","text":"Be terse."}]"#,
        )
        .unwrap();
        assert_eq!(rules[0].class(), RuleClass::Constraint);
        assert_eq!(rules[1].class(), RuleClass::Behavioral);
    }

    #[test]
    fn a_mixed_file_keeps_both_shapes() {
        let rules = parse_rules(
            r#"["bare one", {"class":"constraint","text":"Never disclose credentials"}]"#,
        )
        .unwrap();
        assert_eq!(rules[0].class(), RuleClass::Behavioral);
        assert_eq!(rules[1].class(), RuleClass::Constraint);
    }

    // --- Policy enforcement ---

    #[test]
    fn removing_a_constraint_is_quarantined() {
        let original = vec![
            Rule::constraint("Never disclose credentials"),
            Rule::behavioral("Be terse."),
        ];
        // Optimizer dropped the constraint to satisfy a test.
        let proposed = vec![Rule::behavioral("Be terse.")];

        let result = enforce_policy(
            &original,
            &proposed,
            &policy_with(&["Never disclose credentials"], ProtectedMatch::Exact),
        );

        assert_eq!(result.changes.len(), 1);
        assert_eq!(result.changes[0].rule_text, "Never disclose credentials");
        assert!(
            result
                .accepted
                .iter()
                .any(|r| r.text() == "Never disclose credentials"),
            "the protected rule must be restored: {:?}",
            result.accepted
        );
    }

    #[test]
    fn altering_a_constraint_is_quarantined() {
        let original = vec![Rule::constraint("Never disclose credentials")];
        // Same rule, downgraded to behavioral so the optimizer can edit it later.
        let proposed = vec![Rule::behavioral("Never disclose credentials")];

        let result = enforce_policy(
            &original,
            &proposed,
            &policy_with(&["Never disclose credentials"], ProtectedMatch::Exact),
        );
        assert_eq!(result.changes.len(), 1);
        assert_eq!(
            result.accepted[0].class(),
            RuleClass::Constraint,
            "the constraint class must be restored, not just the text"
        );
    }

    #[test]
    fn behavioral_rules_are_untouched_by_policy() {
        let original = vec![
            Rule::behavioral("Be terse."),
            Rule::behavioral("Cite sources."),
        ];
        let proposed = vec![
            Rule::behavioral("Be terse."),
            Rule::behavioral("Add examples."),
        ];

        let result = enforce_policy(
            &original,
            &proposed,
            &policy_with(&["Never disclose credentials"], ProtectedMatch::Exact),
        );
        assert!(result.is_empty(), "{:?}", result.changes);
        assert_eq!(result.accepted.len(), 2);
        assert!(result.accepted.iter().any(|r| r.text() == "Add examples."));
    }

    #[test]
    fn prefix_match_catches_drifted_text() {
        let policy = policy_with(&["Escalate emergencies to 911"], ProtectedMatch::Prefix);
        let drifted = Rule::behavioral("Escalate emergencies to 911 immediately, without delay");
        assert!(policy.protects(&drifted));
    }

    #[test]
    fn exact_match_does_not_catch_drifted_text() {
        let policy = policy_with(&["Escalate emergencies to 911"], ProtectedMatch::Exact);
        let drifted = Rule::behavioral("Escalate emergencies to 911 immediately, without delay");
        assert!(!policy.protects(&drifted));
    }

    #[test]
    fn a_matching_constraint_is_not_quarantined() {
        let original = vec![Rule::constraint("Never disclose credentials")];
        let proposed = original.clone();
        let result = enforce_policy(
            &original,
            &proposed,
            &policy_with(&["Never disclose credentials"], ProtectedMatch::Exact),
        );
        assert!(result.is_empty(), "{:?}", result.changes);
    }

    #[test]
    fn ordering_is_preserved_so_the_file_stays_diffable() {
        let original = vec![
            Rule::behavioral("first"),
            Rule::constraint("Never disclose credentials"),
            Rule::behavioral("third"),
        ];
        let proposed = vec![
            Rule::behavioral("first"),
            Rule::behavioral("inserted"),
            Rule::behavioral("third"),
        ];
        let result = enforce_policy(
            &original,
            &proposed,
            &policy_with(&["Never disclose credentials"], ProtectedMatch::Exact),
        );
        let texts: Vec<&str> = result.accepted.iter().map(|r| r.text()).collect();
        assert_eq!(
            texts,
            vec!["first", "Never disclose credentials", "third", "inserted"]
        );
    }

    #[test]
    fn empty_policy_is_a_no_op() {
        let original = vec![Rule::behavioral("a")];
        let proposed = vec![Rule::behavioral("b")];
        let result = enforce_policy(&original, &proposed, &RulePolicy::default());
        assert!(result.is_empty());
        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.accepted[0].text(), "b");
    }
}
