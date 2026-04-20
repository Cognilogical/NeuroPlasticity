use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;

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

pub async fn run_copilot_optimizer(
    model_name: &str,
    failing_logs: &str,
    task_prompt: &str,
) -> Result<String> {
    // 1. Get the local GitHub CLI token automatically
    let output = Command::new("gh")
        .arg("auth")
        .arg("token")
        .output()
        .context("Failed to execute gh auth token")?;
    
    let gh_token = String::from_utf8(output.stdout)?.trim().to_string();

    if gh_token.is_empty() {
        anyhow::bail!("GitHub token is empty. Please run `gh auth login`.");
    }

    // 2. Build standard OpenAI-compatible payload for the GitHub endpoint
    let url = "https://models.inference.ai.azure.com/chat/completions";

    let payload = serde_json::json!({
        "model": model_name,
        "messages": [
            {
                "role": "system",
                "content": "You are the NeuroPlasticity Meta-Optimizer. Read the failing test logs and output ONLY a JSON rule to fix the agent's behavior. Format as a string: 'Rule: ...'"
            },
            {
                "role": "user",
                "content": format!("Task: {}\n\nFailing Logs:\n{}", task_prompt, failing_logs)
            }
        ]
    });

    let client = reqwest::Client::new();
    let res = client.post(url)
        .bearer_auth(gh_token)
        .json(&payload)
        .send()
        .await
        .context("Failed to send request to GitHub Models API")?;

    let response_json: serde_json::Value = res.json().await.context("Failed to parse JSON response")?;

    // 3. Extract the mutated rule
    let new_rule = response_json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("Fallback rule: avoid previous mistakes")
        .to_string();

    Ok(new_rule)
}

/// Runs the meta-optimizer and writes a rules overlay if the score is below the pass threshold.
pub async fn run_optimizer(score: f64, pass_threshold: f64, task_prompt: &str, stderr: &str) -> Result<()> {
    if score >= pass_threshold {
        println!("Score {} >= pass threshold {}. No optimization needed.", score, pass_threshold);
        return Ok(());
    }

    println!("Score {} < pass threshold {}. Generating rules overlay...", score, pass_threshold);

    // Call the actual GitHub Copilot Models endpoint
    let new_rule = run_copilot_optimizer("gpt-4o-mini", stderr, task_prompt).await?;

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
