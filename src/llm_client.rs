use anyhow::{Context, Result};
use crate::manifest::MetaLlmConfig;
use tokio::process::Command;

pub async fn ask_llm(
    config: &MetaLlmConfig,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String> {
    if config.provider == "embedded" {
        #[cfg(feature = "embedded-llm")]
        {
            return crate::embedded_llm::run_embedded_llm(system_prompt, user_prompt, config.model_path.as_ref()).await;
        }
        #[cfg(not(feature = "embedded-llm"))]
        {
            anyhow::bail!("The 'embedded' provider requires the 'embedded-llm' feature to be enabled during build.");
        }
    }
    
    let (url, token) = if config.provider == "github" {
        // 1. Get the local GitHub CLI token automatically
        let output = Command::new("gh")
            .arg("auth")
            .arg("token")
            .output()
            .await
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
        
        // P0 Security Fix: Prevent exfiltration of arbitrary host env vars (like AWS_SECRET_ACCESS_KEY or SSH_PRIVATE_KEY) via malicious plasticity.json
        // Hardened: Must exactly match known patterns, not just suffixes
        let is_valid_env_var = env_var == "API_KEY" || 
            env_var == "GITHUB_TOKEN" ||
            env_var == "OPENAI_API_KEY" ||
            env_var == "ANTHROPIC_API_KEY" ||
            env_var == "GEMINI_API_KEY" ||
            env_var == "GROQ_API_KEY" ||
            env_var == "XAI_API_KEY" ||
            env_var == "DEEPSEEK_API_KEY" ||
            env_var == "TOGETHER_API_KEY";
            
        if !is_valid_env_var {
            let re = regex::Regex::new(r"^[A-Z][A-Z0-9_]*_(API_KEY|TOKEN)$").unwrap();
            let is_safe_pattern = re.is_match(env_var) 
                && !env_var.contains("SECRET") 
                && !env_var.contains("PRIVATE") 
                && !env_var.contains("AWS")
                && !env_var.contains("SSH")
                && !env_var.contains("GCP")
                && !env_var.contains("AZURE");
                
            if !is_safe_pattern {
                anyhow::bail!("Security Exception: To prevent credential exfiltration, `api_key_env` must be a standard LLM API key name. Attempted to use: {}", env_var);
            }
        }
        
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
                "content": system_prompt
            },
            {
                "role": "user",
                "content": user_prompt
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

    // 4. Extract the text
    let new_text = response_json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("Fallback error")
        .to_string();

    Ok(new_text)
}
