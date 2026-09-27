use anyhow::{Result, bail};

use crate::llm_client::{CompletionSpec, ask_llm_json};
use crate::manifest::MetaLlmConfig;

/// JSON Schema for the optimizer's reply.
pub fn rule_schema() -> serde_json::Value {
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
    Ok(rule)
}

/// A behavioral rule is injected into the agent's prompt forever, so an
/// unbounded "rule" would silently eat the context window.
const MAX_RULE_CHARS: usize = 400;

pub async fn run_llm_optimizer(
    config: &MetaLlmConfig,
    failing_logs: &str,
    task_prompt: &str,
    existing_rules: &[String],
) -> Result<String> {
    let system_prompt = concat!(
        "You are the NeuroPlasticity Meta-Optimizer. You write one behavioral rule that ",
        "fixes the agent's failure.\n",
        "Reply with a single JSON object: {\"rule\": \"<your rule>\"} and nothing else.\n",
        "The rule must be imperative, at most two sentences, and free of markdown or ",
        "explanatory preamble.\n",
        "DO NOT restate any rule from the Existing Rules array; the agent already failed ",
        "with those rules active."
    );

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

    let user_prompt = format!(
        "Task: {}\n\nExisting Rules Already Attempted (Do not repeat these):\n{}\n\nFailing Logs:\n{}",
        task_prompt, rules_json, truncated_logs
    );

    let spec = CompletionSpec {
        system_prompt,
        user_prompt: &user_prompt,
        json_schema: Some(rule_schema()),
    };

    let value = ask_llm_json(config, &spec).await?;
    let raw = value
        .get("rule")
        .and_then(|r| r.as_str())
        .ok_or_else(|| anyhow::anyhow!("Meta-Optimizer reply had no 'rule' field: {}", value))?;

    let rule = validate_rule(raw)?;

    // The prompt forbids repeats; enforce it, otherwise a looping model
    // appends the same constraint to rules.json every epoch.
    if existing_rules
        .iter()
        .any(|existing| existing.trim().eq_ignore_ascii_case(&rule))
    {
        bail!(
            "Meta-Optimizer repeated an existing rule: \"{}\" — refusing to append a duplicate",
            rule
        );
    }

    Ok(rule)
}
