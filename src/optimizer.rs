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

use crate::manifest::MetaLlmConfig;

pub async fn run_llm_optimizer(
    config: &MetaLlmConfig,
    failing_logs: &str,
    task_prompt: &str,
) -> Result<String> {
    let (url, token) = if config.provider == "github" {
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
        
        ("https://models.inference.ai.azure.com/chat/completions".to_string(), gh_token)
    } else {
        // 2. Generic OpenAI-compatible endpoint
        let url = config.base_url.clone().unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".to_string());
        let env_var = config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
        let api_key = std::env::var(env_var).unwrap_or_default();
        
        if api_key.is_empty() {
            anyhow::bail!("API key environment variable {} is empty.", env_var);
        }
        
        (url, api_key)
    };

    // 3. Build standard OpenAI-compatible payload
    let payload = serde_json::json!({
        "model": config.model,
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
    let res = client.post(&url)
        .bearer_auth(token)
        .json(&payload)
        .send()
        .await
        .context("Failed to send request to LLM API")?;

    let response_json: serde_json::Value = res.json().await.context("Failed to parse JSON response")?;

    // 4. Extract the mutated rule
    let new_rule = response_json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("Fallback rule: avoid previous mistakes")
        .to_string();

    Ok(new_rule)
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
