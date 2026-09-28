use anyhow::{Result, bail};

use crate::llm_client::{CompletionSpec, ask_llm_json};
use crate::manifest::MetaLlmConfig;

/// JSON Schema for the optimizer's reply.
///
/// `unit` is requested only when the failure is attributable to a step (F5);
/// without localization there is no unit to name, and asking for one invites
/// the model to invent an id.
pub fn rule_schema(with_unit: bool) -> serde_json::Value {
    if with_unit {
        serde_json::json!({
            "type": "object",
            "properties": {
                "rule": {
                    "type": "string",
                    "description": "One imperative behavioral constraint scoped to the failing step, at most two sentences, no markdown and no 'Rule:' prefix."
                },
                "unit": {
                    "type": "string",
                    "description": "The id of the step this rule is about."
                }
            },
            "required": ["rule", "unit"],
            "additionalProperties": false
        })
    } else {
        serde_json::json!({
            "type": "object",
            "properties": {
                "rule": {
                    "type": "string",
                    "description": "One imperative behavioral constraint, at most two sentences, no markdown and no 'Rule:' prefix."
                }
            },
            "required": ["rule"],
            "additionalProperties": false
        })
    }
}

/// Sanitize a model-authored rule before it is persisted as a constraint.
pub fn validate_rule(raw: &str) -> Result<String> {
    let mut rule = raw.trim().to_string();

    // Models sometimes echo the format instruction back at us.
    for prefix in ["Rule:", "rule:", "RULE:"] {
        if let Some(stripped) = rule.strip_prefix(prefix) {
            rule = stripped.trim().to_string();
        }
    }
    // Collapse the stray quoting/whitespace that shows up in fenced replies.
    rule = rule
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c.is_whitespace())
        .to_string();

    if rule.is_empty() {
        bail!("Meta-Optimizer returned an empty rule");
    }
    if rule.chars().count() > MAX_RULE_CHARS {
        bail!(
            "Meta-Optimizer returned a {} character rule; it must be at most {} characters",
            rule.chars().count(),
            MAX_RULE_CHARS
        );
    }

    // A small model sometimes echoes the schema instruction back instead of
    // answering it, producing a degenerate repeat. That is a malformed reply,
    // and persisting it would inject ~1KB of instruction text into the agent's
    // prompt on every subsequent epoch.
    if is_degenerate_repeat(&rule) {
        bail!(
            "Meta-Optimizer returned a repeated instruction fragment rather than a rule: \
             \"{}\"",
            truncate_for_error(&rule)
        );
    }
    Ok(rule)
}

/// A behavioral rule is injected into the agent's prompt forever, so an
/// unbounded "rule" would silently eat the context window.
const MAX_RULE_CHARS: usize = 400;

/// Detect a phrase repeated back several times, which signals an echoed
/// instruction rather than a rule.
fn is_degenerate_repeat(rule: &str) -> bool {
    const WINDOW: usize = 6;
    const MIN_REPEATS: usize = 3;

    let words: Vec<&str> = rule.split_whitespace().collect();
    if words.len() < WINDOW * MIN_REPEATS {
        return false;
    }
    // Any WINDOW-word sequence occurring MIN_REPEATS+ times is a repeat loop.
    words
        .windows(WINDOW)
        .any(|first| words.windows(WINDOW).filter(|c| *c == first).count() > MIN_REPEATS)
}

fn truncate_for_error(text: &str) -> String {
    if text.chars().count() <= 80 {
        return text.to_string();
    }
    text.chars().take(80).collect::<String>() + "…"
}

pub async fn run_llm_optimizer(
    config: &MetaLlmConfig,
    failing_logs: &str,
    task_prompt: &str,
    existing_rules: &[crate::rules::Rule],
) -> Result<String> {
    run_optimizer_with_context(config, failing_logs, task_prompt, existing_rules, None)
        .await
        .map(|r| r.text)
}

/// Same, but with an optional localized failure context (F5).
///
/// When an agent emits a transcript and the failure is attributable to one
/// step, the optimizer is shown that step rather than a truncated whole-run
/// blob. A rule written against step 7 can be phrased about step 7; a rule
/// written against a whole-run log is necessarily global, and global rules are
/// how an optimizer fixes one behavior and quietly changes three others.
pub async fn run_optimizer_with_context(
    config: &MetaLlmConfig,
    failing_logs: &str,
    task_prompt: &str,
    existing_rules: &[crate::rules::Rule],
    localized: Option<&str>,
) -> Result<GeneratedRule> {
    let (system_prompt, _localization_note) = match localized {
        Some(_) => (
            concat!(
                "You are the NeuroPlasticity Meta-Optimizer. You write one behavioral rule ",
                "that fixes the agent's failure.\n",
                "Answer with a single JSON object: {\"rule\": \"<your rule>\", ",
                "\"unit\": \"<id>\"} and nothing else.\n",
                "The value of \"rule\" must be plain instruction text, at most two sentences, ",
                "imperative, and free of markdown or preamble. Never describe the format you ",
                "are using, and never repeat these instructions back.\n",
                "The failure is attributable to ONE step. Scope \"rule\" to that step: a rule ",
                "that also changes unrelated behavior is a regression, not a fix. Set \"unit\" ",
                "to the id of the step the rule is about.\n",
                "DO NOT restate any rule from the Existing Rules array; the agent already ",
                "failed with those rules active."
            ),
            "",
        ),
        None => (
            concat!(
                "You are the NeuroPlasticity Meta-Optimizer. You write one behavioral rule ",
                "that fixes the agent's failure.\n",
                "Answer with a single JSON object: {\"rule\": \"<your rule>\"} and nothing ",
                "else.\n",
                "The value of \"rule\" must be plain instruction text, at most two sentences, ",
                "imperative, and free of markdown or preamble. Never describe the format you ",
                "are using, and never repeat these instructions back.\n",
                "DO NOT restate any rule from the Existing Rules array; the agent already ",
                "failed with those rules active."
            ),
            "",
        ),
    };

    let rules_json =
        serde_json::to_string_pretty(existing_rules).unwrap_or_else(|_| "[]".to_string());

    // Truncate logs to prevent LLM context overflow or API rejection (P1 Fix)
    let max_log_len = 8000;
    let truncated_logs = if failing_logs.len() > max_log_len {
        let skip = failing_logs.len() - max_log_len;
        format!("...[TRUNCATED]...\n{}", &failing_logs[skip..])
    } else {
        failing_logs.to_string()
    };

    // F5: when the failure is attributable to a unit, lead with that narrow
    // context so the rule can be phrased narrowly. The whole-run log is still
    // included — localization is additional signal, not a replacement.
    let user_prompt = match localized {
        Some(localized) => format!(
            "Task: {}\n\nExisting Rules Already Attempted (Do not repeat these):\n{}\n\n\
             {}\n\nFull Run Logs (context only; scope your rule to the unit above):\n{}",
            task_prompt, rules_json, localized, truncated_logs
        ),
        None => format!(
            "Task: {}\n\nExisting Rules Already Attempted (Do not repeat these):\n{}\n\nFailing Logs:\n{}",
            task_prompt, rules_json, truncated_logs
        ),
    };
    let spec = CompletionSpec {
        system_prompt,
        user_prompt: &user_prompt,
        json_schema: Some(rule_schema(localized.is_some())),
    };

    let value = ask_llm_json(config, &spec).await?;
    let raw = value
        .get("rule")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!("Meta-Optimizer reply had no 'rule' field: {}", value))?;

    let rule = validate_rule(raw)?;

    // F5: which unit motivated this rule, so a reviewer checks a narrow claim.
    let unit = value
        .get("unit")
        .and_then(|u| u.as_str())
        .map(str::to_string);

    // The prompt forbids repeats; enforce it, otherwise a looping model
    // appends the same constraint to rules.json every epoch.
    if existing_rules
        .iter()
        .any(|existing| existing.text().trim().eq_ignore_ascii_case(&rule))
    {
        bail!(
            "Meta-Optimizer repeated an existing rule: \"{}\" — refusing to append a duplicate",
            rule
        );
    }

    Ok(GeneratedRule { text: rule, unit })
}

/// A rule and, when the failure was attributable, the step it addresses.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedRule {
    pub text: String,
    /// The transcript step this rule is about, if known (F5).
    pub unit: Option<String>,
}
