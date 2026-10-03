use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// TypeSafe (System One / Jev) defaults (design: FEATURE-typesafe-jev-evaluator)
// ---------------------------------------------------------------------------

/// Default TypeSafe endpoint.
///
/// Configurable per evaluator because the owner's free tier is served behind
/// a different base URL (the OpenCode Zen gateway): zen-hosted Jev and
/// first-party Jev are different grader configurations, so `base_url` is
/// fingerprint material exactly like `meta_llm.base_url`.
pub const TYPESAFE_DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// Default TypeSafe model — a PINNED version, never the `jev-latest` alias.
///
/// Two reasons, both about silent grader drift:
/// - Aliases move silently to newer models: the grader would change with no
///   local diff. The *default* is worse still — it is omitted from the
///   serialized evaluator (`skip_serializing_if`), so a moving default would
///   change the grader without even changing the failure fingerprint.
/// - The response reports the version that actually answered, which the
///   runtime records in verdict provenance.
pub const TYPESAFE_DEFAULT_MODEL: &str = "jev-1.13.0";

/// Environment variable holding the TypeSafe API key. Never the manifest
/// itself: keys do not belong in committed configuration.
pub const TYPESAFE_DEFAULT_API_KEY_ENV: &str = "TYPESAFE_API_KEY";

/// p ≥ this → PASS (design: default 0.75).
pub const TYPESAFE_DEFAULT_PASS_ABOVE: f64 = 0.75;

/// p ≤ this → FAIL (design: default 0.35). Between the bands: INDETERMINATE.
pub const TYPESAFE_DEFAULT_FAIL_BELOW: f64 = 0.35;

/// Choice/Score answers whose confidence falls below this are forced to
/// INDETERMINATE (design: default 0.5). Does not apply to Noul answers —
/// they carry no confidence; the probability dead-band *is* the signal.
pub const TYPESAFE_DEFAULT_INDETERMINATE_BELOW: f64 = 0.5;

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
    /// TypeSafe System One ("Jev") — a judgment model that returns typed,
    /// schema-constrained answers with probabilities instead of generated
    /// prose. Deliberately its own evaluator type rather than a provider
    /// branch inside the `llm` path: forcing typed judgments through
    /// chat-shaped plumbing would rebuild the parse-failure class this
    /// provider exists to eliminate (design §Implementation shape).
    Typesafe,
}

/// A manifest evaluator.
///
/// `Default` exists so tests and transforms can build one without spelling
/// out every field; production values always come from the manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    // --- TypeSafe (System One / Jev) configuration -------------------------
    // Flat fields matching the agreed manifest JSON shape. They are harmless
    // (ignored) on `host_bash` / `container` / `llm` evaluators — but they
    // still serialize into the failure fingerprint, which is *correct*:
    // adding or removing them is a grader-configuration change and must
    // invalidate cached failures.
    /// Template for the state text the judgment model judges. Supports
    /// `{{run.transcript}}` and `{{target_file}}`. Absent → the evaluator's
    /// `target_file` content is used as-is (like the `llm` evaluator).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    /// The typed questions to ask, one judgment per entry (Noul / Choice /
    /// Score). Required for `type: "typesafe"` — see [`validate_typesafe`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub questions: Option<BTreeMap<String, TypesafeQuestion>>,
    /// Verdict bands and per-answer routing. Absent → documented defaults
    /// (pass 0.75 / fail 0.35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<TypesafeMapping>,
    /// Confidence floor for Choice/Score answers: below it the verdict is
    /// forced to INDETERMINATE. Absent → 0.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indeterminate_below: Option<f64>,
    /// TypeSafe endpoint. Absent → [`TYPESAFE_DEFAULT_BASE_URL`]. Set this to
    /// the OpenCode Zen gateway for the free tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Model ID. Absent → [`TYPESAFE_DEFAULT_MODEL`] (pinned, never
    /// `jev-latest`: aliases move silently and would change the grader with
    /// no local diff).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Env var holding the API key. Absent → [`TYPESAFE_DEFAULT_API_KEY_ENV`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Load-time fallback: `"llm"` converts this evaluator to the equivalent
    /// `llm` grader for the whole run when the API key is missing — decided
    /// once, before any epoch, never mid-run (so κ stays comparable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
}

impl Evaluator {
    /// Resolve this evaluator's TypeSafe configuration with all defaults
    /// filled in.
    ///
    /// Returns `None` when no `questions` are declared — a config error the
    /// runtime reports as a failed score (see [`validate_typesafe`] for the
    /// full fail-loud check). Not gated on `r#type`: the field set is what
    /// defines the configuration, and callers gate on type themselves.
    pub fn typesafe_spec(&self) -> Option<TypesafeResolved> {
        let questions = self.questions.clone()?;
        let (pass_above, fail_below, verdict_by_answer) = resolve_bands(self.mapping.as_ref());
        Some(TypesafeResolved {
            document: self.document.clone(),
            questions,
            pass_above,
            fail_below,
            verdict_by_answer,
            indeterminate_below: self
                .indeterminate_below
                .unwrap_or(TYPESAFE_DEFAULT_INDETERMINATE_BELOW),
            base_url: self
                .base_url
                .clone()
                .unwrap_or_else(|| TYPESAFE_DEFAULT_BASE_URL.to_string()),
            model: self
                .model
                .clone()
                .unwrap_or_else(|| TYPESAFE_DEFAULT_MODEL.to_string()),
            api_key_env: self
                .api_key_env
                .clone()
                .unwrap_or_else(|| TYPESAFE_DEFAULT_API_KEY_ENV.to_string()),
        })
    }

    /// Distinct `base_url`s this evaluator's TypeSafe traffic would reach —
    /// the evaluator's own endpoint plus any per-grader overrides, so the
    /// egress plan can list every outbound path before a run starts.
    pub fn typesafe_endpoints(&self) -> Vec<String> {
        let inherited = || {
            self.base_url
                .clone()
                .unwrap_or_else(|| TYPESAFE_DEFAULT_BASE_URL.to_string())
        };
        let mut urls = Vec::new();
        if self.r#type == EvaluatorType::Typesafe {
            urls.push(inherited());
        }
        for grader in &self.graders {
            if let Some(ts) = &grader.typesafe {
                let url = ts.base_url.clone().unwrap_or_else(&inherited);
                if !urls.contains(&url) {
                    urls.push(url);
                }
            }
        }
        urls
    }
}

/// Fill the verdict bands from a `mapping`, applying documented defaults.
fn resolve_bands(
    mapping: Option<&TypesafeMapping>,
) -> (f64, f64, Option<BTreeMap<String, String>>) {
    let pass_above = mapping.map_or(TYPESAFE_DEFAULT_PASS_ABOVE, |m| m.pass_above());
    let fail_below = mapping.map_or(TYPESAFE_DEFAULT_FAIL_BELOW, |m| m.fail_below());
    let verdict_by_answer = mapping.and_then(|m| m.verdict_by_answer.clone());
    (pass_above, fail_below, verdict_by_answer)
}

/// Which judgment primitive a TypeSafe question asks (design §API reference).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypesafePrimitive {
    /// Probability the condition holds, 0–1. Carries **no** separate
    /// confidence (verified from docs): the probability itself is the
    /// uncertainty, so the dead-band between the mapping thresholds is the
    /// only uncertainty signal for Noul.
    Noul,
    /// One option from a defined set, with the full probability distribution
    /// and a confidence in 0–1.
    Choice,
    /// A probability-weighted position along ordered rubric levels, with a
    /// confidence in 0–1.
    Score,
}

impl TypesafePrimitive {
    pub fn label(self) -> &'static str {
        match self {
            TypesafePrimitive::Noul => "noul",
            TypesafePrimitive::Choice => "choice",
            TypesafePrimitive::Score => "score",
        }
    }
}

/// One typed judgment question.
///
/// Question text is complete and self-contained by design: meaning lives in
/// the question, not in prompt craft (design §Non-goals).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypesafeQuestion {
    pub primitive: TypesafePrimitive,
    /// The question as put to the judgment model. May contain
    /// `{{run.transcript}}` / `{{target_file}}`, rendered against run state
    /// before the request is sent.
    pub question: String,
    /// Primitive-specific rubric, passed to the API verbatim. Shapes (docs):
    /// - noul → object `{"true": …, "false": …}` — optional;
    /// - choice → map `<option> → rubric string | null` — required;
    /// - score → ordered array of level strings, ≥ 2 levels — required.
    ///
    /// Deliberately an opaque `Value`: the exact rubric grammar is the API's
    /// business, and manifest validation only enforces what changes verdict
    /// semantics locally (presence/level-count), not string shapes the API
    /// already rejects loudly with a 422.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criteria: Option<serde_json::Value>,
}

/// Verdict bands and per-answer routing for TypeSafe questions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TypesafeMapping {
    /// Probability ≥ this → PASS. Absent → 0.75.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_above: Option<f64>,
    /// Probability ≤ this → FAIL. Absent → 0.35. Between the bands →
    /// INDETERMINATE: the dead-band is where the model is unsure, and an
    /// unsure grader must not invent a failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_below: Option<f64>,
    /// Choice answer → `"PASS"` / `"FAIL"` / `"INDETERMINATE"`, for graders
    /// that route on *which* answer was chosen ("which failure class is
    /// this?"). May cover a subset of the choice criteria: answers outside
    /// the map are INDETERMINATE, which is safer than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict_by_answer: Option<BTreeMap<String, String>>,
}

impl TypesafeMapping {
    pub fn pass_above(&self) -> f64 {
        self.pass_above.unwrap_or(TYPESAFE_DEFAULT_PASS_ABOVE)
    }

    pub fn fail_below(&self) -> f64 {
        self.fail_below.unwrap_or(TYPESAFE_DEFAULT_FAIL_BELOW)
    }
}

/// A TypeSafe grader inside a quorum (F4b).
///
/// Deliberately has **no** `document`: every grader in a quorum judges the
/// same state, so one is inherited from the evaluator. And no `fallback`:
/// the fallback is a load-time, whole-evaluator decision made once per run —
/// mixing resolved grader kinds inside one quorum would break κ
/// comparability mid-run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TypesafeGraderSpec {
    /// Required: what this grader judges. The evaluator's `document` (or its
    /// `target_file`) is the state.
    pub questions: BTreeMap<String, TypesafeQuestion>,
    /// Absent → the evaluator's `mapping`, then documented defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mapping: Option<TypesafeMapping>,
    /// Absent → the evaluator's `indeterminate_below`, then 0.5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indeterminate_below: Option<f64>,
    /// Absent → the evaluator's `base_url`, then [`TYPESAFE_DEFAULT_BASE_URL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Absent → the evaluator's `model`, then [`TYPESAFE_DEFAULT_MODEL`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Absent → the evaluator's `api_key_env`, then
    /// [`TYPESAFE_DEFAULT_API_KEY_ENV`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// A TypeSafe configuration with every default resolved — what the runtime
/// paths (evaluator arm and quorum graders) actually execute against.
#[derive(Debug, Clone)]
pub struct TypesafeResolved {
    /// State template, or `None` → grade the `target_file` content.
    pub document: Option<String>,
    pub questions: BTreeMap<String, TypesafeQuestion>,
    pub pass_above: f64,
    pub fail_below: f64,
    pub verdict_by_answer: Option<BTreeMap<String, String>>,
    pub indeterminate_below: f64,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Makes this grader a TypeSafe judgment grader instead of a chat-model
    /// one: it votes with a single combined verdict over the *evaluator's*
    /// document (F4b). When set, `meta_llm` is not consulted for this grader.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typesafe: Option<TypesafeGraderSpec>,
}

impl GraderSpec {
    /// Resolve this grader's TypeSafe configuration, inheriting state and
    /// connection settings from `eval` (a quorum grader judges the same
    /// document through the same endpoint unless it says otherwise).
    ///
    /// `None` when the grader declares no `typesafe` block — such a grader
    /// keeps the pre-existing `meta_llm` behavior.
    pub fn typesafe_spec(&self, eval: &Evaluator) -> Option<TypesafeResolved> {
        let ts = self.typesafe.as_ref()?;
        let (grader_pass, grader_fail, grader_vba) = resolve_bands(ts.mapping.as_ref());
        // A grader-level `mapping` (even an empty `{}`) fully replaces the
        // evaluator's; only absence inherits.
        let (pass_above, fail_below, verdict_by_answer) = if ts.mapping.is_some() {
            (grader_pass, grader_fail, grader_vba)
        } else {
            resolve_bands(eval.mapping.as_ref())
        };
        let indeterminate_below = ts
            .indeterminate_below
            .or(eval.indeterminate_below)
            .unwrap_or(TYPESAFE_DEFAULT_INDETERMINATE_BELOW);
        Some(TypesafeResolved {
            document: eval.document.clone(),
            questions: ts.questions.clone(),
            pass_above,
            fail_below,
            verdict_by_answer,
            indeterminate_below,
            base_url: ts
                .base_url
                .clone()
                .or_else(|| eval.base_url.clone())
                .unwrap_or_else(|| TYPESAFE_DEFAULT_BASE_URL.to_string()),
            model: ts
                .model
                .clone()
                .or_else(|| eval.model.clone())
                .unwrap_or_else(|| TYPESAFE_DEFAULT_MODEL.to_string()),
            api_key_env: ts
                .api_key_env
                .clone()
                .or_else(|| eval.api_key_env.clone())
                .unwrap_or_else(|| TYPESAFE_DEFAULT_API_KEY_ENV.to_string()),
        })
    }
}

/// Fail-loud validation of a TypeSafe evaluator's configuration.
///
/// Meaningful only for `type == "typesafe"`: other evaluator types return
/// `Ok` unconditionally, so callers may check without branching. Every rule
/// here exists because the alternative is grading epochs with a
/// configuration whose verdicts are meaningless — overlapping bands grade
/// one probability both ways, an empty question set grades nothing, a
/// Choice with no criteria has no options to choose from.
///
/// Note what is *not* checked: a Noul's `criteria` shape. The documented
/// example itself uses free-form criteria there, and the API rejects a
/// genuinely malformed body with a loud 422 at request time — a runtime
/// abort is fail-loud enough, while over-strict validation here would
/// reject manifests the design accepts.
pub fn validate_typesafe(eval: &Evaluator) -> Result<(), String> {
    if eval.r#type != EvaluatorType::Typesafe {
        return Ok(());
    }
    let ctx = format!("typesafe evaluator '{}'", eval.name);
    let questions = eval.questions.as_ref().ok_or_else(|| {
        format!(
            "{} declares no `questions` — a typesafe evaluator must say what to judge",
            ctx
        )
    })?;
    validate_typesafe_config(
        &ctx,
        questions,
        eval.mapping.as_ref(),
        eval.indeterminate_below,
    )?;

    // `fallback` is load-time-only and has exactly one supported value; any
    // other string would silently do nothing, which is a config error.
    if let Some(fallback) = eval.fallback.as_deref()
        && fallback != "llm"
    {
        return Err(format!(
            "{} has unsupported `fallback` {:?} — the only supported value is \"llm\"",
            ctx, fallback
        ));
    }

    for grader in &eval.graders {
        if grader.typesafe.is_some() {
            validate_typesafe_grader(grader, eval)?;
        }
    }
    Ok(())
}

/// Validate one TypeSafe quorum grader against the configuration it inherits
/// from `eval`.
///
/// Public because `llm` evaluators may carry typesafe graders, which
/// [`validate_typesafe`] skips by type — the quorum path checks each
/// typesafe grader through this entry point.
pub fn validate_typesafe_grader(grader: &GraderSpec, eval: &Evaluator) -> Result<(), String> {
    let ctx = format!("grader '{}' of evaluator '{}'", grader.name, eval.name);
    let ts = grader
        .typesafe
        .as_ref()
        .ok_or_else(|| format!("{} has no `typesafe` configuration", ctx))?;
    let mapping = ts.mapping.as_ref().or(eval.mapping.as_ref());
    let indeterminate_below = ts.indeterminate_below.or(eval.indeterminate_below);
    validate_typesafe_config(&ctx, &ts.questions, mapping, indeterminate_below)
}

/// The shared rule set: non-empty questions, well-ordered bands, an
/// `indeterminate_below` in (0, 1], routable verdict labels, and
/// primitive-specific criteria.
fn validate_typesafe_config(
    ctx: &str,
    questions: &BTreeMap<String, TypesafeQuestion>,
    mapping: Option<&TypesafeMapping>,
    indeterminate_below: Option<f64>,
) -> Result<(), String> {
    if questions.is_empty() {
        return Err(format!(
            "{} has an empty `questions` map — nothing would be judged",
            ctx
        ));
    }

    let (pass_above, fail_below, verdict_by_answer) = resolve_bands(mapping);
    // Overlapping or inverted bands must fail manifest validation, not grade
    // epochs: the same probability would land in both the PASS and FAIL
    // regions, and which one wins would be an accident of check order.
    // Ordered explicitly rather than as `!(pass_above > fail_below)`: these
    // are floats, so partial ordering is the actual contract (an unordered
    // pair — or a NaN, which JSON cannot express but a hand-built manifest
    // could — is not a usable configuration either).
    if !matches!(
        pass_above.partial_cmp(&fail_below),
        Some(std::cmp::Ordering::Greater)
    ) {
        return Err(format!(
            "{} has inverted or overlapping verdict bands: pass_above ({}) must be strictly \
             greater than fail_below ({})",
            ctx, pass_above, fail_below
        ));
    }

    let indeterminate_below = indeterminate_below.unwrap_or(TYPESAFE_DEFAULT_INDETERMINATE_BELOW);
    if !(indeterminate_below > 0.0 && indeterminate_below <= 1.0) {
        return Err(format!(
            "{} has indeterminate_below {} — it must be greater than 0 and at most 1",
            ctx, indeterminate_below
        ));
    }

    // Same acceptance as `evaluator::Verdict::parse`, kept local so manifest
    // validation does not depend on the evaluator module.
    if let Some(map) = &verdict_by_answer {
        for (answer, verdict) in map {
            let routable = matches!(
                verdict.trim().to_uppercase().as_str(),
                "PASS" | "FAIL" | "INDETERMINATE"
            );
            if !routable {
                return Err(format!(
                    "{} maps choice answer {:?} to {:?} — verdict_by_answer values must be \
                     PASS, FAIL, or INDETERMINATE",
                    ctx, answer, verdict
                ));
            }
        }
    }

    for (id, q) in questions {
        match q.primitive {
            // Noul criteria are optional and their shape is left to the
            // API's 422 (see this function's doc comment).
            TypesafePrimitive::Noul => {}
            TypesafePrimitive::Choice => {
                let criteria = q.criteria.as_ref().ok_or_else(|| {
                    format!(
                        "{} question '{}' is a choice but declares no `criteria` — without the \
                         option set there is nothing to choose from",
                        ctx, id
                    )
                })?;
                let has_options = criteria
                    .as_object()
                    .is_some_and(|options| !options.is_empty());
                if !has_options {
                    return Err(format!(
                        "{} question '{}' is a choice but its `criteria` is not a non-empty \
                         option → rubric map",
                        ctx, id
                    ));
                }
                // A Choice with no answer routing could never produce a
                // decisive verdict: every answer would map to
                // INDETERMINATE, which makes the grader decorative. Partial
                // coverage is fine — unmapped answers *at runtime* are
                // INDETERMINATE, the safer default.
                if verdict_by_answer.as_ref().is_none_or(|m| m.is_empty()) {
                    return Err(format!(
                        "{} question '{}' is a choice but `mapping.verdict_by_answer` is missing \
                         or empty — without it every answer would be INDETERMINATE and this \
                         grader could never decide",
                        ctx, id
                    ));
                }
            }
            TypesafePrimitive::Score => {
                let criteria = q.criteria.as_ref().ok_or_else(|| {
                    format!(
                        "{} question '{}' is a score but declares no `criteria` — without its \
                         ordered levels there is nothing to normalize against",
                        ctx, id
                    )
                })?;
                let levels = criteria.as_array().map_or(0, |a| a.len());
                if levels < 2 {
                    return Err(format!(
                        "{} question '{}' is a score with {} level(s) — a score needs at least \
                         2 ordered levels to normalize a weighted answer",
                        ctx, id, levels
                    ));
                }
            }
        }
    }
    Ok(())
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

    // --- TypeSafe (System One / Jev) --------------------------------------

    /// The exact example shape from the feature design, plus the two fields
    /// every evaluator needs (`name`, `weight`).
    const TYPESAFE_EXAMPLE: &str = r#"{
        "name": "rules_compliance",
        "type": "typesafe",
        "base_url": "https://api.typesafe.ai",
        "model": "jev-1.13.0",
        "document": "{{run.transcript}}",
        "questions": {
            "passes_rule": {
                "primitive": "noul",
                "question": "Does the transcript above comply with the rule stated below? \"{{rule.text}}\"",
                "criteria": "The full conversation is visible; judge only what is shown."
            }
        },
        "mapping": { "pass_above": 0.75, "fail_below": 0.35 },
        "indeterminate_below": 0.5,
        "fallback": "llm",
        "weight": 1.0
    }"#;

    #[test]
    fn a_typesafe_evaluator_parses_from_json() {
        let e: Evaluator = serde_json::from_str(TYPESAFE_EXAMPLE).unwrap();
        assert_eq!(e.r#type, EvaluatorType::Typesafe);
        assert_eq!(e.base_url.as_deref(), Some("https://api.typesafe.ai"));
        assert_eq!(e.model.as_deref(), Some("jev-1.13.0"));
        assert_eq!(e.document.as_deref(), Some("{{run.transcript}}"));
        assert_eq!(e.indeterminate_below, Some(0.5));
        assert_eq!(e.fallback.as_deref(), Some("llm"));

        let questions = e.questions.as_ref().expect("questions must parse");
        let q = &questions["passes_rule"];
        assert_eq!(q.primitive, TypesafePrimitive::Noul);
        assert!(q.question.contains("{{rule.text}}"));

        let mapping = e.mapping.as_ref().expect("mapping must parse");
        assert_eq!(mapping.pass_above, Some(0.75));
        assert_eq!(mapping.fail_below, Some(0.35));
    }

    #[test]
    fn validate_typesafe_accepts_the_documented_example() {
        let e: Evaluator = serde_json::from_str(TYPESAFE_EXAMPLE).unwrap();
        validate_typesafe(&e).expect("the documented example must validate");
    }

    /// Defaults are resolved, not required in the manifest: an evaluator with
    /// only questions still grades with the documented bands.
    #[test]
    fn typesafe_spec_fills_documented_defaults() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "minimal",
                "type": "typesafe",
                "questions": { "ok": { "primitive": "noul", "question": "Is it ok?" } },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let spec = e.typesafe_spec().expect("questions are declared");
        assert_eq!(spec.pass_above, TYPESAFE_DEFAULT_PASS_ABOVE);
        assert_eq!(spec.fail_below, TYPESAFE_DEFAULT_FAIL_BELOW);
        assert_eq!(
            spec.indeterminate_below,
            TYPESAFE_DEFAULT_INDETERMINATE_BELOW
        );
        assert_eq!(spec.base_url, TYPESAFE_DEFAULT_BASE_URL);
        assert_eq!(spec.model, TYPESAFE_DEFAULT_MODEL);
        assert_eq!(spec.api_key_env, TYPESAFE_DEFAULT_API_KEY_ENV);
        assert!(validate_typesafe(&e).is_ok());
    }

    /// Overlapping/inverted bands must fail validation, not grade epochs.
    #[test]
    fn validate_typesafe_rejects_inverted_bands() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "bad_bands",
                "type": "typesafe",
                "questions": { "ok": { "primitive": "noul", "question": "Is it ok?" } },
                "mapping": { "pass_above": 0.3, "fail_below": 0.7 },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("pass_above"), "{}", err);
        assert!(err.contains("fail_below"), "{}", err);
    }

    /// Equal bands leave no room for the dead-band and grade the boundary
    /// probability twice.
    #[test]
    fn validate_typesafe_rejects_equal_bands() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "equal_bands",
                "type": "typesafe",
                "questions": { "ok": { "primitive": "noul", "question": "Is it ok?" } },
                "mapping": { "pass_above": 0.5, "fail_below": 0.5 },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        assert!(validate_typesafe(&e).is_err());
    }

    #[test]
    fn validate_typesafe_rejects_empty_questions() {
        // Missing questions.
        let e: Evaluator =
            serde_json::from_str(r#"{"name": "none", "type": "typesafe", "weight": 1.0}"#).unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("no `questions`"), "{}", err);

        // Present but empty.
        let e: Evaluator = serde_json::from_str(
            r#"{"name": "empty", "type": "typesafe", "questions": {}, "weight": 1.0}"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("empty `questions`"), "{}", err);
    }

    /// A Choice with no verdict routing can only ever answer INDETERMINATE —
    /// decorative, so it is rejected up front.
    #[test]
    fn validate_typesafe_rejects_choice_without_verdict_by_answer() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "route",
                "type": "typesafe",
                "questions": {
                    "class": {
                        "primitive": "choice",
                        "question": "Which failure class is this?",
                        "criteria": { "benign": "Cosmetic only", "broken": "Core flow fails" }
                    }
                },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("verdict_by_answer"), "{}", err);

        // With routing present — even covering only a subset — it validates.
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "route",
                "type": "typesafe",
                "questions": {
                    "class": {
                        "primitive": "choice",
                        "question": "Which failure class is this?",
                        "criteria": { "benign": "Cosmetic only", "broken": "Core flow fails" }
                    }
                },
                "mapping": { "verdict_by_answer": { "benign": "PASS", "broken": "FAIL" } },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        validate_typesafe(&e).expect("routed choice must validate");
    }

    #[test]
    fn validate_typesafe_rejects_unroutable_verdict_labels() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "route",
                "type": "typesafe",
                "questions": {
                    "class": {
                        "primitive": "choice",
                        "question": "Which failure class is this?",
                        "criteria": { "benign": null, "broken": null }
                    }
                },
                "mapping": { "verdict_by_answer": { "benign": "maybe" } },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("PASS, FAIL, or INDETERMINATE"), "{}", err);
    }

    #[test]
    fn validate_typesafe_requires_choice_criteria() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "no_criteria",
                "type": "typesafe",
                "questions": { "class": { "primitive": "choice", "question": "Pick one" } },
                "mapping": { "verdict_by_answer": { "a": "PASS" } },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("`criteria`"), "{}", err);
    }

    #[test]
    fn validate_typesafe_requires_at_least_two_score_levels() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "one_level",
                "type": "typesafe",
                "questions": {
                    "quality": { "primitive": "score", "question": "Rate it", "criteria": ["good"] }
                },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("at least"), "{}", err);

        // Two levels normalize fine (denominator 1).
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "two_levels",
                "type": "typesafe",
                "questions": {
                    "quality": { "primitive": "score", "question": "Rate it", "criteria": ["bad", "good"] }
                },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        validate_typesafe(&e).expect("two levels must validate");
    }

    #[test]
    fn validate_typesafe_rejects_out_of_range_indeterminate_below() {
        for bad in [0.0, -0.1, 1.5] {
            let e: Evaluator = serde_json::from_str(&format!(
                r#"{{
                    "name": "floor",
                    "type": "typesafe",
                    "questions": {{ "ok": {{ "primitive": "noul", "question": "Is it ok?" }} }},
                    "indeterminate_below": {},
                    "weight": 1.0
                }}"#,
                bad
            ))
            .unwrap();
            let err = validate_typesafe(&e).unwrap_err();
            assert!(err.contains("indeterminate_below"), "{}", err);
        }
        // The boundary 1.0 is allowed: every non-certain answer is then
        // INDETERMINATE, which is strict but coherent.
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "floor",
                "type": "typesafe",
                "questions": { "ok": { "primitive": "noul", "question": "Is it ok?" } },
                "indeterminate_below": 1.0,
                "weight": 1.0
            }"#,
        )
        .unwrap();
        validate_typesafe(&e).expect("1.0 is within (0, 1]");
    }

    #[test]
    fn validate_typesafe_rejects_unknown_fallback_values() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "fb",
                "type": "typesafe",
                "questions": { "ok": { "primitive": "noul", "question": "Is it ok?" } },
                "fallback": "gemini",
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let err = validate_typesafe(&e).unwrap_err();
        assert!(err.contains("\"llm\""), "{}", err);
    }

    /// Validation is a no-op for other evaluator types: the flat fields are
    /// ignored there, so rejecting on them would break existing manifests.
    #[test]
    fn validate_typesafe_ignores_non_typesafe_evaluators() {
        let e: Evaluator =
            serde_json::from_str(r#"{"name": "bash", "type": "host_bash", "weight": 1.0}"#)
                .unwrap();
        validate_typesafe(&e).expect("host_bash must be unaffected");
    }

    /// Acceptance test 3's shape: a quorum mixing a typesafe grader with an
    /// llm grader parses and both graders resolve.
    #[test]
    fn a_quorum_mixing_typesafe_and_llm_graders_parses() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "quorum",
                "type": "llm",
                "prompt": "Grade it",
                "target_file": "out.txt",
                "graders": [
                    {
                        "name": "jev",
                        "role": "primary",
                        "typesafe": {
                            "questions": { "passes": { "primitive": "noul", "question": "Passes?" } },
                            "mapping": { "pass_above": 0.8 }
                        }
                    },
                    { "name": "chat", "role": "veto" }
                ],
                "weight": 1.0
            }"#,
        )
        .unwrap();

        let jev = &e.graders[0];
        assert!(jev.typesafe.is_some());
        let spec = jev.typesafe_spec(&e).expect("typesafe grader resolves");
        // Inherited from the evaluator (absent here → documented defaults).
        assert_eq!(spec.pass_above, 0.8, "grader mapping overrides");
        assert_eq!(spec.base_url, TYPESAFE_DEFAULT_BASE_URL);

        // The llm grader keeps its pre-existing behavior.
        assert!(e.graders[1].typesafe.is_none());
        assert!(e.graders[1].typesafe_spec(&e).is_none());
    }

    /// A grader-level mapping fully replaces the evaluator's — partial
    /// inheritance would mix thresholds from two configs into one verdict.
    #[test]
    fn a_grader_mapping_replaces_the_evaluators_entirely() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "quorum",
                "type": "llm",
                "target_file": "out.txt",
                "mapping": { "pass_above": 0.9, "fail_below": 0.1, "verdict_by_answer": { "a": "PASS" } },
                "graders": [
                    {
                        "name": "jev",
                        "role": "primary",
                        "typesafe": {
                            "questions": { "passes": { "primitive": "noul", "question": "Passes?" } },
                            "mapping": { "pass_above": 0.6 }
                        }
                    }
                ],
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let spec = e.graders[0].typesafe_spec(&e).unwrap();
        assert_eq!(spec.pass_above, 0.6, "grader's own pass_above");
        // fail_below comes from the grader's mapping (absent → default), NOT
        // from the evaluator's 0.1.
        assert_eq!(spec.fail_below, TYPESAFE_DEFAULT_FAIL_BELOW);
        assert!(spec.verdict_by_answer.is_none());
    }

    #[test]
    fn grader_typesafe_validation_covers_quorum_graders() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "quorum",
                "type": "llm",
                "target_file": "out.txt",
                "graders": [
                    {
                        "name": "jev",
                        "role": "primary",
                        "typesafe": { "questions": {} }
                    }
                ],
                "weight": 1.0
            }"#,
        )
        .unwrap();
        // An llm evaluator skips validate_typesafe by type…
        validate_typesafe(&e).expect("type is llm");
        // …but the quorum path validates each typesafe grader directly.
        let err = validate_typesafe_grader(&e.graders[0], &e).unwrap_err();
        assert!(err.contains("empty `questions`"), "{}", err);
    }

    // --- Fingerprint material ---------------------------------------------

    /// The failure fingerprint hashes `serde_json::to_string(&evaluators)`,
    /// so this serialization IS the invalidation contract: rewording a
    /// question changes the grader as much as changing a threshold, and must
    /// produce a different hash. These tests document that property for the
    /// TypeSafe fields (fingerprint.rs itself is deliberately untouched).
    #[test]
    fn question_text_is_fingerprint_material() {
        let base = r#"{
            "name": "t",
            "type": "typesafe",
            "questions": { "p": { "primitive": "noul", "question": "%s" } },
            "weight": 1.0
        }"#;
        let a: Evaluator = serde_json::from_str(&base.replace("%s", "Does it comply?")).unwrap();
        let b: Evaluator =
            serde_json::from_str(&base.replace("%s", "Does it strictly comply?")).unwrap();
        assert_ne!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "rewording a question must invalidate the cached failure"
        );
    }

    #[test]
    fn mapping_thresholds_are_fingerprint_material() {
        let a: Evaluator = serde_json::from_str(
            r#"{
                "name": "t", "type": "typesafe",
                "questions": { "p": { "primitive": "noul", "question": "ok?" } },
                "mapping": { "pass_above": 0.75 }, "weight": 1.0
            }"#,
        )
        .unwrap();
        let b: Evaluator = serde_json::from_str(
            r#"{
                "name": "t", "type": "typesafe",
                "questions": { "p": { "primitive": "noul", "question": "ok?" } },
                "mapping": { "pass_above": 0.8 }, "weight": 1.0
            }"#,
        )
        .unwrap();
        assert_ne!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap(),
            "moving a threshold is a different grader configuration"
        );
    }

    #[test]
    fn endpoint_and_model_are_fingerprint_material() {
        let base: Evaluator = serde_json::from_str(
            r#"{
                "name": "t", "type": "typesafe",
                "questions": { "p": { "primitive": "noul", "question": "ok?" } },
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let rehosted = Evaluator {
            base_url: Some("https://zen.example/gateway".to_string()),
            ..base.clone()
        };
        let repinned = Evaluator {
            model: Some("jev-2.0.0".to_string()),
            ..base.clone()
        };
        let original = serde_json::to_string(&base).unwrap();
        assert_ne!(serde_json::to_string(&rehosted).unwrap(), original);
        assert_ne!(serde_json::to_string(&repinned).unwrap(), original);
    }

    /// Unset optional fields are omitted entirely: a manifest that never
    /// mentioned TypeSafe serializes exactly as before, so adding this
    /// feature changed no existing fingerprint.
    #[test]
    fn absent_typesafe_fields_do_not_change_existing_serialization() {
        let e: Evaluator =
            serde_json::from_str(r#"{"name": "bash", "type": "host_bash", "weight": 1.0}"#)
                .unwrap();
        let json = serde_json::to_string(&e).unwrap();
        for key in ["document", "questions", "mapping", "typesafe", "fallback"] {
            assert!(!json.contains(key), "unexpected `{}` in {}", key, json);
        }
    }

    #[test]
    fn typesafe_endpoints_list_every_distinct_outbound_url() {
        let e: Evaluator = serde_json::from_str(
            r#"{
                "name": "q",
                "type": "llm",
                "target_file": "out.txt",
                "base_url": "https://zen.example/gateway",
                "graders": [
                    { "name": "a", "role": "primary", "typesafe": { "questions": { "p": { "primitive": "noul", "question": "ok?" } } } },
                    { "name": "b", "role": "veto", "typesafe": { "base_url": "https://api.typesafe.ai", "questions": { "p": { "primitive": "noul", "question": "ok?" } } } },
                    { "name": "c", "role": "audit", "typesafe": { "questions": { "p": { "primitive": "noul", "question": "ok?" } } } },
                    { "name": "d", "role": "veto" }
                ],
                "weight": 1.0
            }"#,
        )
        .unwrap();
        let urls = e.typesafe_endpoints();
        assert_eq!(urls.len(), 2, "distinct URLs only: {:?}", urls);
        assert!(urls.contains(&"https://zen.example/gateway".to_string()));
        assert!(urls.contains(&"https://api.typesafe.ai".to_string()));

        // No TypeSafe traffic → no endpoints.
        let plain: Evaluator =
            serde_json::from_str(r#"{"name": "bash", "type": "host_bash", "weight": 1.0}"#)
                .unwrap();
        assert!(plain.typesafe_endpoints().is_empty());
    }
}
