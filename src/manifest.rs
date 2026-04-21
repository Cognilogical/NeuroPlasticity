use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlasticityManifest {
    #[serde(rename = "$schema", default)]
    pub schema: Option<String>,
    pub name: String,
    pub task_prompt: String,
    pub agent_command: Vec<String>,
    pub sandbox: Sandbox,
    pub optimization: Optimization,
    #[serde(default)]
    pub evaluators: Vec<Evaluator>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub engine: String,
    pub base_image: String,
    #[serde(default)]
    pub setup_script: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Optimization {
    pub target_rules_file: String,
    pub epochs: u32,
    pub pass_threshold: f64,
    pub meta_llm: MetaLlmConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaLlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model_path: Option<String>,
}

fn default_provider() -> String {
    "github".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluator {
    pub name: String,
    pub script: Vec<String>,
    pub weight: f64,
}
