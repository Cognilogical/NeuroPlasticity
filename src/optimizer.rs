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

/// Runs the meta-optimizer and writes a rules overlay if the score is below the pass threshold.
pub fn run_optimizer(score: f64, pass_threshold: f64, task_prompt: &str, stderr: &str) -> Result<()> {
    if score >= pass_threshold {
        println!("Score {} >= pass threshold {}. No optimization needed.", score, pass_threshold);
        return Ok(());
    }

    println!("Score {} < pass threshold {}. Generating rules overlay...", score, pass_threshold);

    // Dummy/mocked LLM payload, simulating meta_llm parsing task_prompt and failing stderr
    let mocked_rules = vec![
        "Avoid using unwrap() in the critical path.".to_string(),
        "Ensure all file paths are absolute.".to_string(),
        "Handle potential null values explicitly before dereferencing.".to_string(),
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
