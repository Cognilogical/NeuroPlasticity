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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegressionPolicy {
    /// Report the regression and mark the patch REGRESSED, but still emit it.
    /// Preserves throughput; the consumer is warned.
    #[default]
    Annotate,
    /// Refuse to emit rules that regress a previously-passing evaluator.
    Block,
}

/// What to do when a rule that fixed one evaluator broke another (F1).
///
/// Defaults to `annotate` for backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegressionGuard {
    #[serde(default)]
    pub policy: RegressionPolicy,
}

impl Default for RegressionGuard {
    fn default() -> Self {
        Self {
            policy: RegressionPolicy::default(),
        }
    }
}

/// Policy for `target_rules_file`: which rules the optimizer may not touch,
/// and how the file is parsed (F2).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RulesFile {
    #[serde(default)]
    pub policy: crate::rules::RulePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPolicy {
    /// Optional. Absent means the manifest is unclassified (F3).
    #[serde(default)]
    pub data_class: Option<crate::egress::DataClass>,
    /// Optional. Read only when `data_class` is declared.
    #[serde(default)]
    pub egress: Option<crate::egress::EgressPolicy>,
}

impl Default for DataPolicy {
    fn default() -> Self {
        Self {
            data_class: None,
            egress: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Optimization {
    pub target_rules_file: String,
    pub epochs: u32,
    pub pass_threshold: f64,
    pub meta_llm: MetaLlmConfig,
    /// Optional. Absent means `annotate`, which matches pre-existing behavior.
    #[serde(default)]
    pub regression_guard: Option<RegressionGuard>,
    /// Optional. Absent means no protected rules, so nothing is quarantined.
    #[serde(default)]
    pub rules: Option<RulesFile>,
    /// Optional. Absent means unclassified and no egress enforcement (F3).
    #[serde(default)]
    pub data: Option<DataPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaLlmConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model_path: Option<String>,
    /// Sampling temperature. Defaults to 0.0 (deterministic grading) when omitted.
    #[serde(default)]
    pub temperature: Option<f64>,
    /// Max tokens to generate. Defaults to 1024 when omitted.
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Wire protocol for the `custom` provider: `chat_completions` (default)
    /// or `responses`. Inferred from a `/responses` `base_url` when omitted.
    #[serde(default)]
    pub api_style: Option<String>,
}

fn default_provider() -> String {
    "embedded".to_string()
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
