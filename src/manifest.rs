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
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
    #[serde(default)]
    pub mounts: Option<Vec<MountConfig>>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    #[serde(default = "default_project_mount")]
    pub project_mount: String,
    #[serde(default = "default_scratch_mount")]
    pub scratch_mount: String,
}

fn default_project_mount() -> String {
    "/project".to_string()
}

fn default_scratch_mount() -> String {
    "/workspace".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountConfig {
    pub source: String,
    pub target: String,
    #[serde(default = "default_readonly")]
    pub readonly: bool,
}

fn default_readonly() -> bool {
    true
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorType {
    #[default]
    HostBash,
    Container,
    Llm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluator {
    pub name: String,
    #[serde(default)]
    pub r#type: EvaluatorType,
    pub script: Option<Vec<String>>,
    pub image: Option<String>,
    pub command: Option<Vec<String>>,
    pub setup_script: Option<Vec<String>>,
    pub prompt: Option<String>,
    pub target_file: Option<String>,
    pub weight: f64,
}
