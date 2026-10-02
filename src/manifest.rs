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
    /// Environment variables passed into the sandbox container
    /// (-e per entry). First-run defect 2026-10-01: an `env` block in the
    /// manifest was SILENTLY IGNORED (serde default), so configuration the
    /// author believed was live did nothing.
    #[serde(default)]
    pub env: Option<std::collections::BTreeMap<String, String>>,
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

/// Ceiling on the resources one run may consume (F7).
///
/// All fields optional; omitting `budget` means no cap, matching prior behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Budget {
    /// Wall-clock seconds for the whole run.
    #[serde(default)]
    pub max_wall_clock_seconds: Option<u64>,
    /// Cumulative spend in USD. Requires a model that reports cost.
    #[serde(default)]
    pub max_usd: Option<f64>,
    /// What to do when a limit is reached.
    #[serde(default)]
    pub on_exceed: OnExceed,
    /// USD per 1000 prompt tokens, for `max_usd` accounting.
    ///
    /// Deliberately configuration rather than a built-in price table: prices
    /// change constantly, and a stale hardcoded table would make the cap
    /// quietly wrong. Without these, `max_usd` cannot be enforced and the run
    /// says so.
    #[serde(default)]
    pub cost_per_1k_input_usd: Option<f64>,
    /// USD per 1000 completion tokens, for `max_usd` accounting.
    #[serde(default)]
    pub cost_per_1k_output_usd: Option<f64>,
}

impl Budget {
    /// True when a spend cap is declared but cannot actually be enforced.
    pub fn spend_cap_is_inert(&self) -> bool {
        self.max_usd.is_some()
            && (self.cost_per_1k_input_usd.is_none() || self.cost_per_1k_output_usd.is_none())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnExceed {
    /// Stop the run and report it as not verified.
    #[default]
    Halt,
    /// Print a warning and keep going.
    Warn,
}

/// Tracks spend and elapsed time across a run.
#[derive(Debug, Clone)]
pub struct BudgetTracker {
    budget: Budget,
    started: std::time::Instant,
    /// Most recent per-epoch cost in USD, accumulated externally.
    pub spend_usd: f64,
    pub halted: bool,
}

impl BudgetTracker {
    pub fn new(budget: Budget) -> Self {
        Self {
            budget,
            started: std::time::Instant::now(),
            spend_usd: 0.0,
            halted: false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.budget.max_wall_clock_seconds.is_some() || self.budget.max_usd.is_some()
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Record cost incurred this epoch.
    pub fn add_spend(&mut self, usd: f64) {
        if usd > 0.0 {
            self.spend_usd += usd;
        }
    }

    /// Which limit, if any, has been reached.
    pub fn exceeded(&self) -> Option<String> {
        if let Some(max) = self.budget.max_wall_clock_seconds {
            let elapsed = self.elapsed_secs();
            if elapsed > max {
                return Some(format!(
                    "wall clock {}s exceeded the {}s budget",
                    elapsed, max
                ));
            }
        }
        if let Some(max) = self.budget.max_usd {
            if self.spend_usd > max {
                return Some(format!(
                    "spend ${:.4} exceeded the ${:.2} budget",
                    self.spend_usd, max
                ));
            }
        }
        None
    }

    /// Check the budget, applying `on_exceed`. Returns the reason if the run
    /// must stop.
    pub fn check(&mut self) -> Option<String> {
        let reason = self.exceeded()?;
        match self.budget.on_exceed {
            OnExceed::Halt => {
                self.halted = true;
                Some(reason)
            }
            OnExceed::Warn => {
                eprintln!("⚠️ Budget exceeded: {}", reason);
                None
            }
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
    /// Optional. Absent means no cap on spend or time.
    #[serde(default)]
    pub budget: Option<Budget>,
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
    /// Transcript step this evaluator judges (F5). Refers to a step `id` in the
    /// agent's transcript, not a string offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// `example` (default) judges one outcome. `invariant` judges a property
    /// that must hold across the whole run (F6).
    #[serde(default)]
    pub kind: EvaluatorKind,
    /// Required when `kind` is `invariant`. One of `no_action_after_failure`,
    /// `at_most_once`, `no_failed_steps`. Kept to properties decidable from the
    /// transcript alone, so invariants stay deterministic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assert: Option<String>,
    /// Additional graders for this evaluator (F4b). Empty means a single grader
    /// using `optimization.meta_llm`, which is the pre-existing behavior.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub graders: Vec<GraderSpec>,
}

/// A grader's role in a quorum (F4b).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GraderRole {
    /// Decides the verdict.
    #[default]
    Primary,
    /// Can veto a PASS into INDETERMINATE, but cannot assert a PASS.
    Veto,
    /// Recorded for agreement statistics; does not affect the verdict.
    Audit,
}

/// One member of a grader quorum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraderSpec {
    pub name: String,
    #[serde(default)]
    pub role: GraderRole,
    /// Overrides `optimization.meta_llm` for this grader, so a quorum can mix a
    /// strong hosted model with a cheap local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta_llm: Option<MetaLlmConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Whether an evaluator judges a single outcome or a cross-cutting property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorKind {
    #[default]
    Example,
    Invariant,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_budget_means_unlimited() {
        let mut t = BudgetTracker::new(Budget::default());
        assert!(!t.is_active());
        assert_eq!(t.exceeded(), None);
        // Even a large spend is fine when no cap was set.
        t.add_spend(10_000.0);
        assert_eq!(t.exceeded(), None);
    }

    #[test]
    fn spend_over_the_cap_is_detected() {
        let mut t = BudgetTracker::new(Budget {
            max_usd: Some(1.0),
            ..Default::default()
        });
        assert!(t.is_active());
        assert_eq!(t.exceeded(), None);
        t.add_spend(0.4);
        t.add_spend(0.7);
        let reason = t.exceeded().expect("should exceed");
        assert!(reason.contains("spend"), "{}", reason);
    }

    #[test]
    fn a_zero_cap_is_exceeded_by_any_spend() {
        let mut t = BudgetTracker::new(Budget {
            max_usd: Some(0.0),
            ..Default::default()
        });
        t.add_spend(0.01);
        assert!(t.exceeded().is_some());
    }

    #[test]
    fn halt_stops_the_run() {
        let mut t = BudgetTracker::new(Budget {
            max_usd: Some(0.0),
            on_exceed: OnExceed::Halt,
            ..Default::default()
        });
        t.add_spend(1.0);
        assert!(t.check().is_some());
        assert!(t.halted, "the run must be marked halted");
    }

    #[test]
    fn warn_keeps_going() {
        let mut t = BudgetTracker::new(Budget {
            max_usd: Some(0.0),
            on_exceed: OnExceed::Warn,
            ..Default::default()
        });
        t.add_spend(1.0);
        assert_eq!(t.check(), None, "warn must not stop the run");
        assert!(!t.halted);
    }

    #[test]
    fn budget_halts_by_default() {
        assert_eq!(Budget::default().on_exceed, OnExceed::Halt);
    }

    #[test]
    fn budget_parses_from_a_manifest() {
        let b: Budget = serde_json::from_str(
            r#"{"max_wall_clock_seconds": 900, "max_usd": 5.0, "on_exceed": "halt"}"#,
        )
        .unwrap();
        assert_eq!(b.max_wall_clock_seconds, Some(900));
        assert_eq!(b.max_usd, Some(5.0));
        assert_eq!(b.on_exceed, OnExceed::Halt);
    }

    /// A spend cap that cannot be computed looks enforced and is not, so it
    /// must be reported as inert rather than silently ignored.
    #[test]
    fn a_spend_cap_without_prices_is_inert() {
        let b: Budget = serde_json::from_str(r#"{"max_usd": 5.0}"#).unwrap();
        assert!(b.spend_cap_is_inert());
    }

    #[test]
    fn a_spend_cap_with_both_prices_is_enforceable() {
        let b: Budget = serde_json::from_str(
            r#"{"max_usd": 5.0, "cost_per_1k_input_usd": 0.0001, "cost_per_1k_output_usd": 0.0002}"#,
        )
        .unwrap();
        assert!(!b.spend_cap_is_inert());
    }

    #[test]
    fn no_spend_cap_is_not_inert() {
        let b: Budget = serde_json::from_str(r#"{"max_wall_clock_seconds": 60}"#).unwrap();
        assert!(!b.spend_cap_is_inert());
    }

    /// One price configured is still not enough to compute a cost.
    #[test]
    fn a_partial_price_config_is_still_inert() {
        let b: Budget =
            serde_json::from_str(r#"{"max_usd": 5.0, "cost_per_1k_input_usd": 0.0001}"#).unwrap();
        assert!(b.spend_cap_is_inert());
    }
}
