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
    existing_rules: &[String],
) -> Result<String> {
    let system_prompt = "You are the NeuroPlasticity Meta-Optimizer. Your job is to generate a new behavioral rule to fix the agent's failure. 
You must ONLY output a valid JSON string (or just plain text). Keep it under 2 sentences.
Format your output strictly as: 'Rule: <your rule>'
DO NOT suggest rules that are already in the Existing Rules array. The agent already failed with those rules active.";
    
    let rules_json = serde_json::to_string_pretty(existing_rules).unwrap_or_else(|_| "[]".to_string());
    
    // Truncate logs to prevent LLM context overflow or API rejection (P1 Fix)
    let max_log_len = 8000;
    let truncated_logs = if failing_logs.len() > max_log_len {
        let skip = failing_logs.len() - max_log_len;
        format!("...[TRUNCATED]...\n{}", &failing_logs[skip..])
    } else {
        failing_logs.to_string()
    };
    
    let user_prompt = format!("Task: {}\n\nExisting Rules Already Attempted (Do not repeat these):\n{}\n\nFailing Logs:\n{}", task_prompt, rules_json, truncated_logs);
    
    ask_llm(config, system_prompt, &user_prompt).await
}
