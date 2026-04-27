use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Serialize, Deserialize, Debug)]
pub struct RuleSet {
    pub rules: Vec<String>,
    pub metadata: Metadata,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Metadata {
    pub generation_reason: String,
    pub original_score: f64,
}

use crate::manifest::MetaLlmConfig;
use crate::llm_client::ask_llm;

pub async fn run_llm_optimizer(
    config: &MetaLlmConfig,
    failing_logs: &str,
    task_prompt: &str,
) -> Result<String> {
    let system_prompt = "You are the NeuroPlasticity Meta-Optimizer. Read the failing test logs and output ONLY a JSON rule to fix the agent's behavior. Format as a string: 'Rule: ...'";
    let user_prompt = format!("Task: {}\n\nFailing Logs:\n{}", task_prompt, failing_logs);
    
    ask_llm(config, system_prompt, &user_prompt).await
}

/// Runs the meta-optimizer and writes a rules overlay if the score is below the pass threshold.
pub async fn run_optimizer(score: f64, pass_threshold: f64, task_prompt: &str, stderr: &str, config: &MetaLlmConfig) -> Result<()> {
    if score >= pass_threshold {
        println!("Score {} >= pass threshold {}. No optimization needed.", score, pass_threshold);
        return Ok(());
    }

    println!("Score {} < pass threshold {}. Generating rules overlay...", score, pass_threshold);

    // Call the dynamic provider
    let new_rule = run_llm_optimizer(config, stderr, task_prompt).await?;

    let mocked_rules = vec![
        new_rule,
    ];

    let rule_set = RuleSet {
        rules: mocked_rules,
        metadata: Metadata {
            generation_reason: format!("Generated due to failing task: '{}', Error output: '{}'", task_prompt, stderr),
            original_score: score,
        },
    };

    let target_dir = Path::new(".neuroplasticity");
    if !target_dir.exists() {
        fs::create_dir_all(target_dir).context("Failed to create .neuroplasticity directory")?;
    }

    let rules_path = target_dir.join("rules.json");
    let rules_json = serde_json::to_string_pretty(&rule_set).context("Failed to serialize rule set")?;

    fs::write(&rules_path, rules_json).context("Failed to write rules.json")?;

    println!("Rules overlay successfully written to {:?}", rules_path);

    Ok(())
}
