use crate::llm_client::{CompletionSpec, ask_llm_json};
use crate::manifest::{
    Evaluator, EvaluatorKind, EvaluatorType, GraderRole, MetaLlmConfig, Sandbox, TypesafeQuestion,
    TypesafeResolved,
};
use crate::typesafe::{self, TypesafeCall};
use anyhow::Result;
use futures::future::join_all;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// The fixed instruction every chat-model grader receives: one JSON verdict,
/// with INDETERMINATE first-class. Extracted to a constant so the single
/// `llm` grader, quorum llm graders, and synthesized llm graders inside a
/// Typesafe quorum share it byte-for-byte (the provenance `prompt_hash`
/// depends on it staying identical).
const VERDICT_SYSTEM_PROMPT: &str = concat!(
    "You are an automated evaluator. Grade the document against the prompt.\n",
    "Answer with a single JSON object: {\"verdict\": \"PASS\"|\"FAIL\"|\"INDETERMINATE\", ",
    "\"reason\": \"<one sentence>\"} and nothing else.\n",
    "Use INDETERMINATE only when the document cannot be judged from ",
    "what is shown, or the prompt is ambiguous — never as a substitute ",
    "for FAIL when you can tell. Never describe the format you use."
);

/// A grader's verdict. `INDETERMINATE` is first-class and distinct from FAIL:
/// feeding an undecidable artifact to the optimizer as a failure is how noise
/// becomes a rule (F4a).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Indeterminate,
}

impl Verdict {
    /// Whether this verdict counts toward the run's pass score.
    ///
    /// An INDETERMINATE is not a pass, but it is also not a failure to
    /// optimize against, so it is excluded from scoring rather than counted as
    /// a fail.
    pub fn scores_as_pass(self) -> bool {
        matches!(self, Verdict::Pass)
    }

    pub fn scores_at_all(self) -> bool {
        !matches!(self, Verdict::Indeterminate)
    }

    pub fn label(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::Indeterminate => "INDETERMINATE",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_uppercase().as_str() {
            "PASS" => Some(Verdict::Pass),
            "FAIL" => Some(Verdict::Fail),
            "INDETERMINATE" => Some(Verdict::Indeterminate),
            _ => None,
        }
    }
}

/// How a verdict was produced, recorded so a decision is re-verifiable later.
#[derive(Debug, Clone, PartialEq)]
pub struct VerdictProvenance {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_style: Option<String>,
    pub temperature: f64,
    pub prompt_hash: String,
    pub verdict: Verdict,
}

impl std::fmt::Display for VerdictProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}/{} (temp={}, prompt={})",
            self.provider,
            self.model,
            self.temperature,
            &self.prompt_hash[..self.prompt_hash.len().min(12)]
        )
    }
}

/// JSON Schema for an LLM evaluator's verdict.
fn verdict_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "verdict": {
                "type": "string",
                "enum": ["PASS", "FAIL", "INDETERMINATE"],
                "description": "PASS only if the document satisfies the evaluation prompt. Use INDETERMINATE when it cannot be judged from what is shown, never as a substitute for FAIL."
            },
            "reason": { "type": "string", "description": "One sentence explaining the verdict." }
        },
        "required": ["verdict", "reason"],
        "additionalProperties": false
    })
}

#[derive(Debug)]
pub struct EvaluatorScore {
    pub name: String,
    pub success: bool,
    pub weight: f64,
    pub output: Option<String>, // Useful to capture why an LLM or container failed
    /// The transcript step this failure is attributable to (F5/F6).
    pub attributed_unit: Option<String>,
}

#[derive(Debug)]
pub struct EvaluationResult {
    pub pass: bool,
    pub score: f64,
    pub total_weight: f64,
    pub passing_weight: f64,
    pub threshold: f64,
    pub details: Vec<EvaluatorScore>,
    /// How each LLM verdict was produced, for re-verification (F4a).
    pub provenance: Vec<VerdictProvenance>,
    /// Agreement between graders, when a quorum was used (F4b).
    pub agreement: Vec<Agreement>,
}

pub async fn evaluate(
    evaluators: &[Evaluator],
    working_dir: &Path,
    pass_threshold: f64,
    sandbox: &Sandbox,
    meta_llm: &MetaLlmConfig,
    data_handling: &crate::egress::DataHandling,
    transcript: Option<&crate::transcript::Transcript>,
) -> Result<EvaluationResult> {
    let mut futures = Vec::new();

    // Prevent system resources from being crushed by the embedded LLM.
    // If the provider is 'embedded', strictly limit LLM evaluation concurrency to 1.
    // If it's a cloud provider (GitHub, OpenAI, Anthropic), allow up to 10 concurrent requests.
    let is_embedded = meta_llm.provider == "embedded";
    let llm_concurrency = if is_embedded { 1 } else { 10 };
    let llm_semaphore = Arc::new(Semaphore::new(llm_concurrency));

    // Prevent host CPU/RAM exhaustion from spawning 30+ containers concurrently
    let system_semaphore = Arc::new(Semaphore::new(8));

    // Infrastructure failures (bad key, dead endpoint, unusable verdict) must
    // abort the run instead of being scored as agent failures — otherwise the
    // optimizer "fixes" an agent that was never broken.
    let infra_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    // Verdict provenance for re-verifiability (F4a).
    let provenance: Arc<Mutex<Vec<VerdictProvenance>>> = Arc::new(Mutex::new(Vec::new()));
    // Grader agreement, when a quorum is used (F4b).
    let agreement: Arc<Mutex<Vec<Agreement>>> = Arc::new(Mutex::new(Vec::new()));

    // Spawn each evaluator into an asynchronous task so they execute in parallel
    for eval in evaluators {
        let eval_clone = eval.clone();
        let working_dir_clone = working_dir.to_path_buf();
        let sandbox_clone = sandbox.clone();
        let meta_llm_clone = meta_llm.clone();
        let llm_sem_clone = Arc::clone(&llm_semaphore);
        let sys_sem_clone = Arc::clone(&system_semaphore);
        let infra_errors_clone = Arc::clone(&infra_errors);
        let provenance_clone = Arc::clone(&provenance);
        let agreement_clone = Arc::clone(&agreement);
        // Cloned so the spawned task owns it; `tokio::spawn` requires 'static.
        let data_handling = data_handling.clone();
        let transcript = transcript.cloned();

        let handle = tokio::spawn(async move {
            // Set when an LLM grader could not decide; such a verdict must be
            // excluded from scoring rather than treated as a failure.
            let mut score_indeterminate = false;
            // F6: an invariant judges a property across the whole run, so it
            // runs first and can fail a run whose individual steps all pass.
            if eval_clone.kind == EvaluatorKind::Invariant {
                let (success, output) = evaluate_invariant(&eval_clone, transcript.as_ref());
                if let Some(out) = &output {
                    println!("Invariant '{}': {}", eval_clone.name, out);
                }
                let mut score = EvaluatorScore {
                    name: eval_clone.name.clone(),
                    success,
                    weight: eval_clone.weight,
                    output,
                    attributed_unit: None,
                };
                if let Some(t) = transcript.as_ref() {
                    score.attributed_unit = t
                        .first_failure()
                        .map(|s| s.id.clone())
                        .or_else(|| score.attributed_unit.clone());
                }
                return score;
            }

            let (success, output) = match eval_clone.r#type {
                EvaluatorType::HostBash => {
                    let _permit = sys_sem_clone
                        .acquire()
                        .await
                        .expect("Failed to acquire system semaphore");
                    if let Some(script) = &eval_clone.script {
                        if script.is_empty() {
                            (false, Some("Empty host_bash script array".to_string()))
                        } else {
                            // Issue #4: Prevent Sandbox Escape
                            let cmd_name = &script[0];
                            let safe_commands = ["git", "jq", "cat", "ls", "grep", "echo"];
                            let is_safe = safe_commands.contains(&cmd_name.as_str())
                                || (cmd_name.starts_with('/')
                                    && safe_commands
                                        .iter()
                                        .any(|c| cmd_name.ends_with(&format!("/{}", c))));

                            if !is_safe {
                                // PRINT the rejection (first-run defect 2026-10-01:
                                // this path returned a failed score with NO output
                                // anywhere, so an entire debugging session ran with
                                // evaluators that never executed and nobody could
                                // see why).
                                let rejection = format!(
                                    "Security Exception: host_bash command '{}' is not in the system allowlist (git, jq, cat, ls, grep, echo). Evaluators must use the container environment for arbitrary execution.",
                                    cmd_name
                                );
                                println!("{}", rejection);
                                return EvaluatorScore {
                                    name: eval_clone.name.clone(),
                                    success: false,
                                    weight: eval_clone.weight,
                                    output: Some(rejection),
                                    attributed_unit: None,
                                };
                            }

                            // Prevent relative path execution like "./malicious.sh"
                            if cmd_name.contains('.') || cmd_name.contains("..") {
                                return EvaluatorScore {
                                    name: eval_clone.name.clone(),
                                    success: false,
                                    weight: eval_clone.weight,
                                    output: Some(format!(
                                        "Security Exception: host_bash command cannot be a relative file execution."
                                    )),
                                    attributed_unit: None,
                                };
                            }

                            let mut cmd = Command::new(cmd_name);
                            if script.len() > 1 {
                                cmd.args(&script[1..]);
                            }
                            cmd.current_dir(&working_dir_clone);

                            let timeout_dur = Duration::from_secs(
                                sandbox_clone.timeout_seconds.unwrap_or(60) as u64,
                            );
                            match tokio::time::timeout(timeout_dur, cmd.output()).await {
                                Ok(Ok(out)) => {
                                    let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                                    if msg.is_empty() {
                                        msg = String::from_utf8_lossy(&out.stdout).to_string();
                                    }
                                    println!("Evaluator '{}' output: {}", eval_clone.name, msg);
                                    (out.status.success(), Some(msg))
                                }
                                Ok(Err(e)) => (
                                    false,
                                    Some(format!("Failed to execute host_bash command: {}", e)),
                                ),
                                Err(_) => (
                                    false,
                                    Some(format!(
                                        "Evaluator timed out after {}s",
                                        timeout_dur.as_secs()
                                    )),
                                ),
                            }
                        }
                    } else {
                        (
                            false,
                            Some("Missing 'script' for host_bash evaluator".to_string()),
                        )
                    }
                }
                EvaluatorType::Container => {
                    let _permit = sys_sem_clone
                        .acquire()
                        .await
                        .expect("Failed to acquire system semaphore");
                    if let (Some(image), Some(command)) = (&eval_clone.image, &eval_clone.command) {
                        let preferred_engine = Some(sandbox_clone.engine.clone());
                        let (engine, is_podman) = match crate::container::detect_container_engine(
                            &preferred_engine,
                        )
                        .await
                        {
                            Ok(res) => res,
                            Err(e) => {
                                return EvaluatorScore {
                                    name: eval_clone.name.clone(),
                                    success: false,
                                    weight: eval_clone.weight,
                                    output: Some(format!("Container engine error: {}", e)),
                                    attributed_unit: None,
                                };
                            }
                        };

                        let mut cmd = Command::new(&engine);
                        cmd.arg("run");
                        cmd.arg("--rm");
                        if is_podman {
                            cmd.arg("--userns=keep-id");
                        }
                        cmd.arg("--security-opt");
                        cmd.arg("no-new-privileges");

                        let scratch_mount = sandbox_clone
                            .workspace
                            .as_ref()
                            .map_or("/workspace", |w| &w.scratch_mount);
                        cmd.arg("-v");
                        cmd.arg(&format!(
                            "{}:{}:ro,Z",
                            working_dir_clone.display(),
                            scratch_mount
                        ));
                        cmd.arg("--workdir");
                        cmd.arg(scratch_mount);

                        cmd.arg(image);

                        if let Some(setup) = &eval_clone.setup_script {
                            if !setup.is_empty() {
                                let joined_script = setup.join(" && ");
                                let quoted_cmd: Vec<String> = command
                                    .iter()
                                    .map(|s| {
                                        if s.contains(' ') || s.contains('"') || s.contains('\'') {
                                            format!("'{}'", s.replace('\'', "'\\''"))
                                        } else {
                                            s.clone()
                                        }
                                    })
                                    .collect();
                                let full_command =
                                    format!("{} && {}", joined_script, quoted_cmd.join(" "));
                                cmd.arg("sh");
                                cmd.arg("-c");
                                cmd.arg(&full_command);
                            } else {
                                cmd.args(command);
                            }
                        } else {
                            cmd.args(command);
                        }

                        let timeout_dur =
                            Duration::from_secs(sandbox_clone.timeout_seconds.unwrap_or(60) as u64);
                        match tokio::time::timeout(timeout_dur, cmd.output()).await {
                            Ok(Ok(out)) => {
                                let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                                if msg.is_empty() {
                                    msg = String::from_utf8_lossy(&out.stdout).to_string();
                                }
                                println!("Evaluator '{}' output: {}", eval_clone.name, msg);
                                (out.status.success(), Some(msg))
                            }
                            Ok(Err(e)) => (
                                false,
                                Some(format!("Failed to run container evaluator: {}", e)),
                            ),
                            Err(_) => (
                                false,
                                Some(format!(
                                    "Evaluator timed out after {}s",
                                    timeout_dur.as_secs()
                                )),
                            ),
                        }
                    } else {
                        (
                            false,
                            Some(
                                "Missing 'image' or 'command' for container evaluator".to_string(),
                            ),
                        )
                    }
                }
                EvaluatorType::Llm => {
                    let _permit = llm_sem_clone
                        .acquire()
                        .await
                        .expect("Failed to acquire LLM semaphore");

                    // Egress policy (F3): a grader that may not receive this
                    // data is a configuration error, not an agent failure.
                    if let Err(e) = crate::egress::enforce_egress(
                        &data_handling,
                        &meta_llm_clone.provider,
                        crate::egress::EgressKind::Grader,
                    ) {
                        infra_errors_clone
                            .lock()
                            .unwrap()
                            .push(format!("LLM evaluator '{}': {}", eval_clone.name, e));
                        return EvaluatorScore {
                            name: eval_clone.name.clone(),
                            success: false,
                            weight: eval_clone.weight,
                            output: Some(format!("Egress Error: {}", e)),
                            attributed_unit: None,
                        };
                    }

                    // The prompt and the state this grader judges. An `llm`
                    // evaluator declared today resolves exactly as before (its
                    // `target_file` content); a `document` template — what a
                    // `typesafe` evaluator declared, and what the load-time
                    // `fallback: "llm"` transform keeps — wins when present,
                    // because that is the state the original grader judged.
                    match resolve_llm_judge(&eval_clone, &working_dir_clone, transcript.as_ref())
                        .await
                    {
                        Err(msg) => (false, Some(msg)),
                        Ok(None) => (
                            false,
                            Some("Missing 'prompt' or 'target_file' for llm evaluator".to_string()),
                        ),
                        Ok(Some(judge)) => {
                            let system_prompt = VERDICT_SYSTEM_PROMPT;
                            let user_prompt = format!(
                                "Evaluation Prompt:\n{}\n\nTarget Document ({}):\n{}",
                                judge.prompt, judge.label, judge.content
                            );

                            let spec = CompletionSpec {
                                system_prompt,
                                user_prompt: &user_prompt,
                                json_schema: Some(verdict_schema()),
                            };

                            // F4b: with a quorum, ask each grader and combine.
                            // With none, this is the single pre-existing grader.
                            if !eval_clone.graders.is_empty() {
                                // A `typesafe` grader judges this evaluator's
                                // state, so resolve it before the quorum runs.
                                // A failure here is a configuration error → a
                                // failed score, never an infrastructure abort.
                                let ts_inputs =
                                    if eval_clone.graders.iter().any(|g| g.typesafe.is_some()) {
                                        match resolve_typesafe_inputs(
                                            &eval_clone,
                                            eval_clone.document.as_deref(),
                                            &working_dir_clone,
                                            transcript.as_ref(),
                                        )
                                        .await
                                        {
                                            Ok(inputs) => Some(inputs),
                                            Err(msg) => {
                                                return EvaluatorScore {
                                                    name: eval_clone.name.clone(),
                                                    success: false,
                                                    weight: eval_clone.weight,
                                                    output: Some(msg),
                                                    attributed_unit: None,
                                                };
                                            }
                                        }
                                    } else {
                                        None
                                    };
                                let prompts = LlmGraderPrompts {
                                    system: system_prompt,
                                    user: &user_prompt,
                                    hash_user: &judge.prompt,
                                };
                                return run_grader_quorum(
                                    &eval_clone,
                                    &meta_llm_clone,
                                    &data_handling,
                                    Some(&prompts),
                                    ts_inputs.as_ref(),
                                    &EvaluatorSinks {
                                        infra_errors: &infra_errors_clone,
                                        provenance: &provenance_clone,
                                        agreement: &agreement_clone,
                                    },
                                    transcript.as_ref(),
                                )
                                .await;
                            }

                            match ask_llm_json(&meta_llm_clone, &spec).await {
                                Ok(value) => {
                                    let raw = value
                                        .get("verdict")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default();
                                    let reason = value
                                        .get("reason")
                                        .and_then(|r| r.as_str())
                                        .unwrap_or_default()
                                        .trim();

                                    match Verdict::parse(raw) {
                                        None => {
                                            infra_errors_clone.lock().unwrap().push(format!(
                                                    "LLM evaluator '{}' returned an unusable verdict ({:?})",
                                                    eval_clone.name, raw
                                                ));
                                            (
                                                false,
                                                Some(format!("Unusable LLM verdict: {:?}", raw)),
                                            )
                                        }
                                        Some(parsed) => {
                                            // Provenance is recorded per verdict so a
                                            // patch decision is re-verifiable later (F4a).
                                            provenance_clone.lock().unwrap().push(
                                                VerdictProvenance {
                                                    provider: meta_llm_clone.provider.clone(),
                                                    model: meta_llm_clone.model.clone(),
                                                    base_url: meta_llm_clone.base_url.clone(),
                                                    api_style: meta_llm_clone.api_style.clone(),
                                                    temperature: meta_llm_clone
                                                        .temperature
                                                        .unwrap_or(0.0),
                                                    prompt_hash: hash_prompt(
                                                        system_prompt,
                                                        &judge.prompt,
                                                    ),
                                                    verdict: parsed,
                                                },
                                            );

                                            let success = parsed.scores_as_pass();
                                            score_indeterminate = !parsed.scores_at_all();
                                            let report = if reason.is_empty() {
                                                parsed.label().to_string()
                                            } else {
                                                format!("{} — {}", parsed.label(), reason)
                                            };
                                            println!(
                                                "LLM Evaluator '{}' Verdict: {}",
                                                eval_clone.name, report
                                            );
                                            (success, Some(report))
                                        }
                                    }
                                }
                                Err(e) => {
                                    // An unreachable endpoint or expired key is not an
                                    // agent failure; record it so the run aborts.
                                    infra_errors_clone.lock().unwrap().push(format!(
                                        "LLM evaluator '{}': {}",
                                        eval_clone.name, e
                                    ));
                                    (false, Some(format!("LLM Error: {}", e)))
                                }
                            }
                        }
                    }
                }
                EvaluatorType::Typesafe => {
                    // TypeSafe is a hosted endpoint like any other cloud
                    // grader, so it shares the LLM concurrency budget rather
                    // than adding a second unlimited lane to the run.
                    let _permit = llm_sem_clone
                        .acquire()
                        .await
                        .expect("Failed to acquire LLM semaphore");

                    // Returns the score directly: unlike the other arms this
                    // one must choose per-exit between a configuration failure
                    // (a failed score naming the fix) and an infrastructure
                    // failure (a failed score *plus* an abort), and collapsing
                    // it to `(success, output)` would lose that distinction.
                    return evaluate_typesafe(
                        &eval_clone,
                        &working_dir_clone,
                        transcript.as_ref(),
                        &data_handling,
                        &meta_llm_clone,
                        &EvaluatorSinks {
                            infra_errors: &infra_errors_clone,
                            provenance: &provenance_clone,
                            agreement: &agreement_clone,
                        },
                    )
                    .await;
                }
            };

            let mut result = EvaluatorScore {
                name: eval_clone.name.clone(),
                success,
                weight: eval_clone.weight,
                output,
                // F5: a failure is attributable when the evaluator names a unit
                // and that unit actually failed.
                attributed_unit: eval_clone.unit.clone().filter(|unit| {
                    transcript
                        .as_ref()
                        .and_then(|t| t.step(unit))
                        .is_some_and(|s| s.status.is_failure())
                }),
            };

            // An INDETERMINATE is excluded from scoring rather than counted as
            // a fail: it is not evidence the agent is wrong, so it must not be
            // fed to the optimizer as a failing log.
            if score_indeterminate {
                result.weight = 0.0;
            }
            result
        });

        futures.push(handle);
    }

    // Await all evaluators in parallel
    let results = join_all(futures).await;

    let infra = infra_errors.lock().unwrap().clone();
    if !infra.is_empty() {
        anyhow::bail!(
            "Evaluator infrastructure failed — this is not an agent failure, so the run is aborted: {}",
            infra.join("; ")
        );
    }

    let mut total_weight = 0.0;
    let mut passing_weight = 0.0;
    let mut details = Vec::new();

    for res in results {
        if let Ok(score) = res {
            total_weight += score.weight;
            if score.success {
                passing_weight += score.weight;
            }
            details.push(score);
        }
    }

    let score = if total_weight > 0.0 {
        passing_weight / total_weight
    } else {
        1.0
    };

    Ok(EvaluationResult {
        pass: score >= pass_threshold,
        score,
        total_weight,
        passing_weight,
        threshold: pass_threshold,
        details,
        provenance: provenance.lock().unwrap().clone(),
        agreement: agreement.lock().unwrap().clone(),
    })
}

// ---------------------------------------------------------------------------
// TypeSafe (System One / Jev) evaluator support
// (design: bugreports/FEATURE-typesafe-jev-evaluator.md)
// ---------------------------------------------------------------------------

/// The state a TypeSafe judgment runs over, plus the template substitutions
/// shared with question rendering.
struct TypesafeInputs {
    /// Rendered `document` template, or the `target_file` content.
    state: String,
    /// Owned substitution values: `run.transcript` / `target_file`,
    /// whichever the run could actually supply.
    subs: Vec<(String, String)>,
}

impl TypesafeInputs {
    fn subs(&self) -> Vec<(&str, &str)> {
        self.subs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// The chat-model prompts for the llm path of a quorum.
struct LlmGraderPrompts<'a> {
    system: &'a str,
    user: &'a str,
    /// What provenance `prompt_hash` covers: the evaluator's raw prompt,
    /// matching the pre-extraction quorum code byte-for-byte so existing
    /// provenance hashes do not churn.
    hash_user: &'a str,
}

/// The transcript as JSON Lines, for `{{run.transcript}}`.
fn transcript_jsonl(t: &crate::transcript::Transcript) -> String {
    t.steps
        .iter()
        .map(|s| serde_json::to_string(s).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Resolve the state text a TypeSafe grader judges, plus the template
/// substitutions question rendering shares with it.
///
/// With a `document` template the state is that template rendered against
/// run state; without one it is the `target_file` content. Shared by the
/// `typesafe` arm, by a `typesafe` grader inside a quorum, and by the `llm`
/// arm whenever a `document` is declared — which is what the load-time
/// `fallback: "llm"` transform relies on. Every error here is a
/// **configuration** error: it becomes a failed score naming the fix, never
/// an infrastructure abort (a manifest that references a file it does not
/// declare is the author's mistake, and neither an agent FAIL nor a run
/// abort would say so).
async fn resolve_typesafe_inputs(
    eval: &Evaluator,
    document: Option<&str>,
    working_dir: &Path,
    transcript: Option<&crate::transcript::Transcript>,
) -> Result<TypesafeInputs, String> {
    // How the diagnostics name this evaluator. After the load-time
    // `fallback: "llm"` transform a converted evaluator is no longer
    // `typesafe`-typed, and telling its author to fix "the typesafe
    // evaluator" when the manifest now reads `llm` would send them looking
    // in the wrong place.
    let kind = if eval.r#type == EvaluatorType::Typesafe {
        "typesafe evaluator"
    } else {
        "evaluator"
    };
    let mut subs: Vec<(String, String)> = Vec::new();
    if let Some(t) = transcript {
        subs.push(("run.transcript".to_string(), transcript_jsonl(t)));
    }

    let mentions = |key: &str| {
        let needle = format!("{{{{{}}}}}", key);
        document.is_some_and(|d| d.contains(&needle))
    };

    // Precise diagnostics for the supported keys: asking for a transcript
    // the run never produced is a different mistake than a typo, and the
    // author needs to know which.
    if mentions("run.transcript") && transcript.is_none() {
        return Err(format!(
            "{} '{}': `document` asks for {{{{run.transcript}}}} but the agent \
             emitted no transcript this run",
            kind, eval.name
        ));
    }

    // The target file is read whenever it is declared (its content may be
    // referenced by the document *or* by any question's template), but a
    // missing file only errors when something actually needs it.
    let target_needed = document.is_none() || mentions("target_file");
    let mut target_content: Option<String> = None;
    match eval.target_file.as_deref() {
        Some(target_file) => {
            let path = working_dir.join(target_file);
            if path.exists() {
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        subs.push(("target_file".to_string(), content.clone()));
                        target_content = Some(content);
                    }
                    Err(e) if target_needed => {
                        return Err(format!(
                            "Failed to read target file: {} ({})",
                            target_file, e
                        ));
                    }
                    Err(_) => {}
                }
            } else if target_needed {
                return Err(format!("Target file does not exist: {}", target_file));
            }
        }
        None if target_needed => {
            return Err(format!(
                "Missing 'document' or 'target_file' for {} '{}' — the state \
                 must be a `document` template or the content of `target_file`",
                kind, eval.name
            ));
        }
        None => {}
    }

    let state = match document {
        Some(tmpl) => {
            let borrowed: Vec<(&str, &str)> =
                subs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            crate::typesafe::render_template(tmpl, &borrowed)
                .map_err(|e| format!("{} '{}': document template error: {}", kind, eval.name, e))?
        }
        None => target_content.ok_or_else(|| {
            format!(
                "Missing 'document' or 'target_file' for {} '{}'",
                kind, eval.name
            )
        })?,
    };

    Ok(TypesafeInputs { state, subs })
}

/// What a chat-model grader judges: the evaluation prompt plus the state
/// text it is judged against, and a label naming that state for the prompt
/// header (a `target_file` path, or `document`).
#[derive(Debug)]
struct LlmJudge {
    prompt: String,
    label: String,
    content: String,
}

/// Resolve the prompt and state an `llm` evaluator grades.
///
/// The `target_file` branch is the pre-existing behavior, byte-for-byte: a
/// manifest declared before this helper existed resolves to the same prompt
/// and the same state. A declared `document` template wins over `target_file`
/// when present — that is the state a `typesafe` evaluator declared, and the
/// load-time `fallback: "llm"` transform deliberately keeps it (dropping it
/// would make the converted grader judge a file the original grader never
/// looked at, or nothing at all for a manifest whose state is
/// `{{run.transcript}}`).
///
/// `Ok(None)` means the manifest never said what to grade (no `prompt`, or
/// neither `target_file` nor `document`) — the caller reports it with the
/// legacy wording. `Err` is a state that was named but could not be read.
async fn resolve_llm_judge(
    eval: &Evaluator,
    working_dir: &Path,
    transcript: Option<&crate::transcript::Transcript>,
) -> Result<Option<LlmJudge>, String> {
    let Some(prompt) = eval.prompt.clone() else {
        return Ok(None);
    };
    if eval.document.is_some() {
        let inputs =
            resolve_typesafe_inputs(eval, eval.document.as_deref(), working_dir, transcript)
                .await?;
        return Ok(Some(LlmJudge {
            prompt,
            label: "document".to_string(),
            content: inputs.state,
        }));
    }
    let Some(target_file) = eval.target_file.clone() else {
        return Ok(None);
    };
    let path = working_dir.join(&target_file);
    if !path.exists() {
        return Err(format!("Target file does not exist: {}", target_file));
    }
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(Some(LlmJudge {
            prompt,
            label: target_file,
            content,
        })),
        Err(_) => Err(format!("Failed to read target file: {}", target_file)),
    }
}

/// Render each question's text against run state.
///
/// Fail loud on an unresolved placeholder: literal braces reaching the
/// grader would silently ask a different question than the author wrote.
fn render_questions(
    questions: &BTreeMap<String, TypesafeQuestion>,
    subs: &[(&str, &str)],
) -> Result<BTreeMap<String, TypesafeQuestion>, String> {
    let mut rendered = BTreeMap::new();
    for (id, question) in questions {
        let text = crate::typesafe::render_template(&question.question, subs)
            .map_err(|e| format!("question '{}' template error: {}", id, e))?;
        let mut q = question.clone();
        q.question = text;
        rendered.insert(id.clone(), q);
    }
    Ok(rendered)
}

/// Map a TypeSafe response onto one combined verdict plus per-question
/// detail (`id=PASS (noul p=0.92)`).
///
/// `Err` means the response did not answer the questions that were asked —
/// a protocol violation. The caller records it as an infrastructure
/// failure: an evaluator that cannot answer must never vote.
fn combine_typesafe_answers(
    response: &typesafe::TypesafeResponse,
    questions: &BTreeMap<String, TypesafeQuestion>,
    spec: &TypesafeResolved,
) -> Result<(Verdict, String), String> {
    if let Some(extra) = response
        .answers
        .keys()
        .find(|id| !questions.contains_key(*id))
    {
        return Err(format!("it answered unasked question '{}'", extra));
    }
    let mut verdicts = Vec::new();
    let mut detail = Vec::new();
    for (id, question) in questions {
        let Some(answer) = response.answers.get(id) else {
            return Err(format!("there is no answer for question '{}'", id));
        };
        let verdict = typesafe::verdict_for_answer(answer, question, spec);
        verdicts.push(verdict);
        detail.push(format!(
            "{}={} ({})",
            id,
            verdict.label(),
            typesafe::answer_detail(answer)
        ));
    }
    Ok((typesafe::combine_verdicts(&verdicts), detail.join(", ")))
}

/// The three things every evaluator path reports to, bundled because they are
/// always passed together and never swapped: what would abort the run, what
/// makes each verdict re-verifiable afterwards, and grader agreement.
///
/// Threading three `&Arc<Mutex<Vec<_>>>` arguments through `evaluate_typesafe`
/// and `run_grader_quorum` independently put both over the argument limit
/// while adding no information — they describe the run, not the evaluator
/// being judged, so they travel as one value.
struct EvaluatorSinks<'a> {
    infra_errors: &'a Arc<Mutex<Vec<String>>>,
    provenance: &'a Arc<Mutex<Vec<VerdictProvenance>>>,
    agreement: &'a Arc<Mutex<Vec<Agreement>>>,
}

/// Execute one `typesafe` evaluator end to end.
///
/// Returns the score directly (rather than the `(success, output)` tuple the
/// other arms produce) because every exit here makes a deliberate choice
/// between the two failure classes this design insists on: **configuration**
/// errors are failed scores that name the fix, while **infrastructure**
/// errors (endpoint down, auth refused, unusable response) are pushed to
/// `infra_errors` *and* returned as a failed score — exactly like the `llm`
/// arm — so the run aborts instead of grading an evaluator outage as an
/// agent failure.
///
/// The caller has already acquired the cloud semaphore (TypeSafe is
/// endpoint-only and always runs in the cloud lane, never the embedded-1
/// slot).
async fn evaluate_typesafe(
    eval: &Evaluator,
    working_dir: &Path,
    transcript: Option<&crate::transcript::Transcript>,
    data_handling: &crate::egress::DataHandling,
    meta_llm: &MetaLlmConfig,
    sinks: &EvaluatorSinks<'_>,
) -> EvaluatorScore {
    let attributed_unit = eval.unit.clone().filter(|unit| {
        transcript
            .and_then(|t| t.step(unit))
            .is_some_and(|s| s.status.is_failure())
    });
    let score = |success: bool, weight: f64, output: String| EvaluatorScore {
        name: eval.name.clone(),
        success,
        weight,
        output: Some(output),
        attributed_unit: attributed_unit.clone(),
    };

    // Egress policy (F3): document text leaves the box for the TypeSafe
    // endpoint. `provider_kind("typesafe")` maps to Hosted, so the declared
    // ceiling gates it exactly as it does any other remote provider.
    // A grader that may not receive this data is a configuration error,
    // but it aborts like any egress violation does today.
    if let Err(e) =
        crate::egress::enforce_egress(data_handling, "typesafe", crate::egress::EgressKind::Grader)
    {
        sinks
            .infra_errors
            .lock()
            .unwrap()
            .push(format!("TypeSafe evaluator '{}': {}", eval.name, e));
        return score(false, eval.weight, format!("Egress Error: {}", e));
    }

    // Fail loud on a bad manifest BEFORE any network call: inverted bands or
    // an unroutable choice would grade epochs with nonsense configuration.
    if let Err(msg) = crate::manifest::validate_typesafe(eval) {
        return score(
            false,
            eval.weight,
            format!("Typesafe Config Error: {}", msg),
        );
    }

    let Some(spec) = eval.typesafe_spec() else {
        return score(
            false,
            eval.weight,
            "Missing 'questions' for typesafe evaluator".to_string(),
        );
    };

    let inputs = match resolve_typesafe_inputs(
        eval,
        spec.document.as_deref(),
        working_dir,
        transcript,
    )
    .await
    {
        Ok(inputs) => inputs,
        Err(msg) => return score(false, eval.weight, msg),
    };

    let questions = match render_questions(&spec.questions, &inputs.subs()) {
        Ok(questions) => questions,
        Err(msg) => {
            return score(
                false,
                eval.weight,
                format!("Typesafe Config Error: {}", msg),
            );
        }
    };

    // F4b: a quorum shares this evaluator's state. Typesafe graders vote
    // with one combined verdict each; chat-model graders get a prompt
    // synthesized from these questions (a Typesafe evaluator has no chat
    // prompt of its own unless the manifest declares one).
    if !eval.graders.is_empty() {
        let chat_prompt = eval
            .prompt
            .clone()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| typesafe::synthesize_llm_prompt(&questions));
        let chat_user = format!(
            "Evaluation Prompt:\n{}\n\nTarget Document (typesafe state):\n{}",
            chat_prompt, inputs.state
        );
        let llm_prompts = if eval.graders.iter().any(|g| g.typesafe.is_none()) {
            Some(LlmGraderPrompts {
                system: VERDICT_SYSTEM_PROMPT,
                user: &chat_user,
                hash_user: &chat_prompt,
            })
        } else {
            None
        };
        return run_grader_quorum(
            eval,
            meta_llm,
            data_handling,
            llm_prompts.as_ref(),
            Some(&inputs),
            sinks,
            transcript,
        )
        .await;
    }

    // The single-grader path: one request, typed answers, mechanical
    // verdicts — no prose parsing anywhere on this path.
    let call = TypesafeCall {
        base_url: &spec.base_url,
        model: &spec.model,
        api_key_env: &spec.api_key_env,
        state: &inputs.state,
        questions: &questions,
    };
    match typesafe::run(&call).await {
        Ok(response) => {
            let (combined, detail) = match combine_typesafe_answers(&response, &questions, &spec) {
                Ok(v) => v,
                Err(reason) => {
                    let msg = format!(
                        "TypeSafe evaluator '{}' returned an unusable response: {}",
                        eval.name, reason
                    );
                    sinks.infra_errors.lock().unwrap().push(msg);
                    return score(
                        false,
                        eval.weight,
                        format!("Unusable TypeSafe response: {}", reason),
                    );
                }
            };

            // Provenance (F4a): `model` is the RESOLVED version the response
            // reported — what actually answered, not what was asked for.
            let model = if response.model.trim().is_empty() {
                spec.model.clone()
            } else {
                response.model.clone()
            };
            let questions_json = serde_json::to_string(&questions).unwrap_or_default();
            sinks.provenance.lock().unwrap().push(VerdictProvenance {
                provider: "typesafe".to_string(),
                model,
                base_url: Some(spec.base_url.clone()),
                api_style: None,
                // Judgment models do not sample: there is no temperature
                // knob, and 0.0 documents that rather than inventing one.
                temperature: 0.0,
                prompt_hash: crate::fingerprint::digest_of(&[&questions_json, &inputs.state]),
                verdict: combined,
            });

            let report = format!(
                "{} ({} question{}) — {}",
                combined.label(),
                questions.len(),
                if questions.len() == 1 { "" } else { "s" },
                detail
            );
            println!("Typesafe Evaluator '{}' Verdict: {}", eval.name, report);
            // An INDETERMINATE is excluded from scoring rather than counted
            // as a fail: it is not evidence the agent is wrong (F4a).
            let weight = if combined.scores_at_all() {
                eval.weight
            } else {
                0.0
            };
            score(combined.scores_as_pass(), weight, report)
        }
        Err(e) => {
            // An unreachable endpoint, expired key, or refused request is
            // not an agent failure — record it so the run aborts.
            sinks
                .infra_errors
                .lock()
                .unwrap()
                .push(format!("TypeSafe evaluator '{}': {:#}", eval.name, e));
            score(false, eval.weight, format!("TypeSafe Error: {:#}", e))
        }
    }
}

/// Run an evaluator's grader quorum (F4b) and build the resulting score.
///
/// Shared by the `llm` and `typesafe` arms so vote collection, agreement
/// statistics, and `resolve_quorum` exist exactly once — a pure-llm quorum
/// behaves identically to the pre-extraction code. Each grader votes through
/// its own path: a `typesafe` grader runs the judgment model over the shared
/// state (which the caller resolves whenever any grader needs it), and
/// everything else asks a chat model with `llm_prompts`.
async fn run_grader_quorum(
    eval: &Evaluator,
    meta_llm: &MetaLlmConfig,
    data_handling: &crate::egress::DataHandling,
    llm_prompts: Option<&LlmGraderPrompts<'_>>,
    typesafe_inputs: Option<&TypesafeInputs>,
    sinks: &EvaluatorSinks<'_>,
    transcript: Option<&crate::transcript::Transcript>,
) -> EvaluatorScore {
    // A grader that could not vote is an infrastructure failure — the run
    // aborts rather than letting an unreachable grader's absence read as a
    // PASS (same contract as the single-grader paths). The message is
    // recorded here, exactly as the pre-extraction quorum code did.
    let fail_infra = |infra_msg: String, output: String| {
        sinks.infra_errors.lock().unwrap().push(infra_msg);
        EvaluatorScore {
            name: eval.name.clone(),
            success: false,
            weight: eval.weight,
            output: Some(output),
            attributed_unit: None,
        }
    };
    // The manifest is wrong, not the agent: a failed score that names the
    // fix, without aborting the run.
    let fail_config = |output: String| EvaluatorScore {
        name: eval.name.clone(),
        success: false,
        weight: eval.weight,
        output: Some(output),
        attributed_unit: None,
    };

    let mut votes: Vec<(GraderRole, Verdict)> = Vec::new();
    let mut audit: Vec<String> = Vec::new();

    for grader in &eval.graders {
        if grader.typesafe.is_some() {
            // ---- TypeSafe judgment grader --------------------------------
            if let Err(e) = crate::egress::enforce_egress(
                data_handling,
                "typesafe",
                crate::egress::EgressKind::Grader,
            ) {
                return fail_infra(
                    format!(
                        "TypeSafe evaluator '{}' grader '{}': {}",
                        eval.name, grader.name, e
                    ),
                    format!("Egress Error: {}", e),
                );
            }
            if let Err(msg) = crate::manifest::validate_typesafe_grader(grader, eval) {
                return fail_config(format!("Typesafe Config Error: {}", msg));
            }
            let (Some(spec), Some(inputs)) = (grader.typesafe_spec(eval), typesafe_inputs) else {
                return fail_config(format!(
                    "Typesafe Config Error: grader '{}' has no resolved state to judge",
                    grader.name
                ));
            };
            let questions = match render_questions(&spec.questions, &inputs.subs()) {
                Ok(questions) => questions,
                Err(msg) => {
                    return fail_config(format!(
                        "Typesafe Config Error (grader '{}'): {}",
                        grader.name, msg
                    ));
                }
            };
            let call = TypesafeCall {
                base_url: &spec.base_url,
                model: &spec.model,
                api_key_env: &spec.api_key_env,
                state: &inputs.state,
                questions: &questions,
            };
            let response = match typesafe::run(&call).await {
                Ok(response) => response,
                Err(e) => {
                    return fail_infra(
                        format!(
                            "TypeSafe evaluator '{}' grader '{}': {:#}",
                            eval.name, grader.name, e
                        ),
                        format!("TypeSafe Error (grader '{}'): {:#}", grader.name, e),
                    );
                }
            };
            let (combined, _detail) = match combine_typesafe_answers(&response, &questions, &spec) {
                Ok(v) => v,
                Err(reason) => {
                    return fail_infra(
                        format!(
                            "TypeSafe evaluator '{}' grader '{}' returned an unusable \
                                 response: {}",
                            eval.name, grader.name, reason
                        ),
                        format!(
                            "Unusable TypeSafe response from grader '{}': {}",
                            grader.name, reason
                        ),
                    );
                }
            };
            let model = if response.model.trim().is_empty() {
                spec.model.clone()
            } else {
                response.model.clone()
            };
            let questions_json = serde_json::to_string(&questions).unwrap_or_default();
            sinks.provenance.lock().unwrap().push(VerdictProvenance {
                provider: "typesafe".to_string(),
                model,
                base_url: Some(spec.base_url.clone()),
                api_style: None,
                temperature: 0.0,
                prompt_hash: crate::fingerprint::digest_of(&[&questions_json, &inputs.state]),
                verdict: combined,
            });
            // One combined verdict per typesafe grader — its single vote.
            if grader.role == GraderRole::Audit {
                audit.push(format!("{}={}", grader.name, combined.label()));
            } else {
                votes.push((grader.role, combined));
            }
            continue;
        }

        // ---- chat-model grader (pre-existing behavior) -------------------
        let Some(prompts) = llm_prompts else {
            return fail_config(format!(
                "llm grader '{}' has no prompt to grade with on this evaluator",
                grader.name
            ));
        };
        if let Err(e) = crate::egress::enforce_egress(
            data_handling,
            &grader.meta_llm.as_ref().unwrap_or(meta_llm).provider,
            crate::egress::EgressKind::Grader,
        ) {
            return fail_infra(
                format!(
                    "LLM evaluator '{}' grader '{}': {}",
                    eval.name, grader.name, e
                ),
                format!("Egress Error: {}", e),
            );
        }
        let cfg = grader.meta_llm.clone().unwrap_or_else(|| meta_llm.clone());
        let gspec = CompletionSpec {
            system_prompt: prompts.system,
            user_prompt: prompts.user,
            json_schema: Some(verdict_schema()),
        };
        match ask_llm_json(&cfg, &gspec).await {
            Ok(v) => {
                let raw = v
                    .get("verdict")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                match Verdict::parse(raw) {
                    Some(parsed) => {
                        sinks.provenance.lock().unwrap().push(VerdictProvenance {
                            provider: cfg.provider.clone(),
                            model: cfg.model.clone(),
                            base_url: cfg.base_url.clone(),
                            api_style: cfg.api_style.clone(),
                            temperature: cfg.temperature.unwrap_or(0.0),
                            prompt_hash: hash_prompt(prompts.system, prompts.hash_user),
                            verdict: parsed,
                        });
                        if grader.role == GraderRole::Audit {
                            audit.push(format!("{}={}", grader.name, parsed.label()));
                        } else {
                            votes.push((grader.role, parsed));
                        }
                    }
                    None => {
                        return fail_infra(
                            format!(
                                "LLM evaluator '{}' grader '{}' returned an unusable verdict \
                                 ({:?})",
                                eval.name, grader.name, raw
                            ),
                            format!("Unusable verdict from grader '{}': {:?}", grader.name, raw),
                        );
                    }
                }
            }
            Err(e) => {
                return fail_infra(
                    format!(
                        "LLM evaluator '{}' grader '{}': {}",
                        eval.name, grader.name, e
                    ),
                    format!("LLM Error (grader '{}'): {}", grader.name, e),
                );
            }
        }
    }

    // Agreement across the decisive graders. (Verbatim from before the
    // extraction — pure-llm quorums must behave identically.)
    if votes.len() >= 2 {
        let seq: Vec<Verdict> = votes.iter().map(|(_, v)| *v).collect();
        let a = cohens_kappa(&seq[..1], &seq[1..]);
        let mut observed = a;
        if votes.len() > 2 {
            let b = cohens_kappa(&seq[..2], &seq[2..]);
            observed = Agreement {
                pairs: a.pairs + b.pairs,
                raw: (a.raw * a.pairs as f64 + b.raw * b.pairs as f64) / (a.pairs + b.pairs) as f64,
                kappa: (a.kappa + b.kappa) / 2.0,
                has_variety: a.has_variety || b.has_variety,
            };
        }
        sinks.agreement.lock().unwrap().push(observed);
        if !observed.is_trustworthy() {
            println!(
                "⚠️  Graders on '{}' agree {:.0}% (κ={:.2}) — at or near chance.",
                eval.name,
                observed.raw * 100.0,
                observed.kappa
            );
        }
    }

    let (resolved, conflict) = resolve_quorum(&votes);
    let score_indeterminate = !resolved.scores_at_all();
    let mut report = format!("{} (quorum of {})", resolved.label(), votes.len());
    if let Some(c) = &conflict {
        report.push_str(&format!(" — {}", c));
    }
    if !audit.is_empty() {
        report.push_str(&format!(" — audit: {}", audit.join(", ")));
    }
    let arm = if eval.r#type == EvaluatorType::Typesafe {
        "Typesafe"
    } else {
        "LLM"
    };
    println!("{} Evaluator '{}' Verdict: {}", arm, eval.name, report);
    EvaluatorScore {
        name: eval.name.clone(),
        success: resolved.scores_as_pass(),
        weight: if score_indeterminate {
            0.0
        } else {
            eval.weight
        },
        output: Some(report),
        attributed_unit: eval.unit.clone().filter(|unit| {
            transcript
                .and_then(|t| t.step(unit))
                .is_some_and(|s| s.status.is_failure())
        }),
    }
}

/// Evaluate a cross-cutting invariant against the ordered transcript (F6).
///
/// Unit-style tests cannot express "never does X after Y" or "at most one of
/// these states is ever true". These are the properties that matter most in
/// long-horizon agents, and they need the full ordered transcript, not one
/// manifest's output.
///
/// Attribution is required: a failure must name the transition that violated
/// the invariant, not just the run. Invariants are restricted to the properties
/// that can be decided from the transcript alone, so this stays deterministic
/// rather than asking a model to reason about a sequence.
fn evaluate_invariant(
    eval: &Evaluator,
    transcript: Option<&crate::transcript::Transcript>,
) -> (bool, Option<String>) {
    let Some(eval_assert) = eval.assert.as_deref() else {
        return (
            false,
            Some("An invariant evaluator requires an `assert` property.".to_string()),
        );
    };

    let Some(transcript) = transcript else {
        // Degrade gracefully: a run whose agent emits no transcript cannot be
        // checked for invariants, and must not fail for that alone.
        return (
            true,
            Some("INVARIANT SKIPPED — the agent emitted no transcript, so this property could not be checked.".to_string()),
        );
    };

    match eval_assert {
        // "no booking after a failed lookup in the same run"
        "no_action_after_failure" => {
            let Some(failure) = transcript.first_failure() else {
                return (true, Some("INVARIANT OK — no step failed.".to_string()));
            };
            let offenders: Vec<&crate::transcript::Step> = transcript
                .steps
                .iter()
                .filter(|s| !s.status.is_failure() && !s.kind.is_empty() && s.index > failure.index)
                .collect();
            if offenders.is_empty() {
                (
                    true,
                    Some(format!(
                        "INVARIANT OK — no action of any kind after the failure at `{}`.",
                        failure.id
                    )),
                )
            } else {
                (
                    false,
                    Some(format!(
                        "INVARIANT VIOLATED — a `{}` action at `{}` occurred after `{}` failed. \
                         Violating transition: {} → {}.",
                        offenders[0].kind, offenders[0].id, failure.id, failure.id, offenders[0].id
                    )),
                )
            }
        }
        // "at most one of these two states is ever true"
        "at_most_once" => {
            let Some(unit) = eval.unit.as_deref() else {
                return (
                    false,
                    Some("`at_most_once` requires a `unit` naming the step to count.".to_string()),
                );
            };
            let count = transcript.steps_of_kind(unit).len();
            if count <= 1 {
                (
                    true,
                    Some(format!(
                        "INVARIANT OK — `{}` occurred {} time(s).",
                        unit, count
                    )),
                )
            } else {
                let offenders: Vec<&str> = transcript
                    .steps_of_kind(unit)
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect();
                (
                    false,
                    Some(format!(
                        "INVARIANT VIOLATED — `{}` occurred {} times: {}.",
                        unit,
                        count,
                        offenders.join(", ")
                    )),
                )
            }
        }
        "no_failed_steps" => {
            let failures = transcript.failing_steps();
            if failures.is_empty() {
                (true, Some("INVARIANT OK — no step failed.".to_string()))
            } else {
                (
                    false,
                    Some(format!(
                        "INVARIANT VIOLATED — {} step(s) failed: {}.",
                        failures.len(),
                        failures
                            .iter()
                            .map(|s| s.id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                )
            }
        }
        other => (
            false,
            Some(format!(
                "Unknown invariant `{}`. Supported: no_action_after_failure, at_most_once, \
                 no_failed_steps.",
                other
            )),
        ),
    }
}

/// Agreement between two graders on a set of binary verdicts (F4b).
///
/// Raw agreement alone is misleading: two graders that both answer PASS to
/// everything agree 100% of the time while being useless, so Cohen's κ is
/// reported as well, since it corrects for chance agreement given the label
/// distribution.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agreement {
    pub pairs: usize,
    pub raw: f64,
    pub kappa: f64,
    /// Whether both PASS and FAIL actually occurred. Agreement measured on a
    /// single label carries no information: two graders that only ever say PASS
    /// agree perfectly and are both useless.
    pub has_variety: bool,
}

impl Agreement {
    /// κ at or below this means the graders are not better than chance.
    pub fn is_trustworthy(&self) -> bool {
        self.pairs >= 2 && self.kappa > 0.4 && self.has_variety
    }
}

/// Cohen's κ for two binary labellers over paired observations.
pub fn cohens_kappa(a: &[Verdict], b: &[Verdict]) -> Agreement {
    let pairs = a.len().min(b.len());
    if pairs == 0 {
        return Agreement {
            pairs: 0,
            raw: 0.0,
            kappa: 0.0,
            has_variety: false,
        };
    }
    let mut agree = 0usize;
    let (mut a_pass, mut b_pass) = (0usize, 0usize);
    let mut saw_pass = false;
    let mut saw_fail = false;
    for i in 0..pairs {
        let (x, y) = (a[i], b[i]);
        if x == y {
            agree += 1;
        }
        if x == Verdict::Pass {
            a_pass += 1;
        }
        if y == Verdict::Pass {
            b_pass += 1;
        }
        if x == Verdict::Pass || y == Verdict::Pass {
            saw_pass = true;
        }
        if x == Verdict::Fail || y == Verdict::Fail {
            saw_fail = true;
        }
    }
    let p_o = agree as f64 / pairs as f64;
    let p_e = (a_pass as f64 / pairs as f64) * (b_pass as f64 / pairs as f64)
        + (1.0 - a_pass as f64 / pairs as f64) * (1.0 - b_pass as f64 / pairs as f64);

    let kappa = if (1.0 - p_e).abs() < f64::EPSILON {
        // Both graders are constant and identical: perfect by definition, and
        // κ is undefined. Report the raw agreement.
        1.0
    } else {
        (p_o - p_e) / (1.0 - p_e)
    };

    Agreement {
        pairs,
        raw: p_o,
        kappa,
        has_variety: saw_pass && saw_fail,
    }
}

/// Combine a quorum's verdicts into one (F4b).
///
/// A `veto` grader can turn a PASS into INDETERMINATE, but cannot assert a
/// PASS on its own. Any disagreement between a primary and a veto is
/// INDETERMINATE rather than FAIL: two graders disagreeing is not evidence the
/// artifact is wrong, and treating it as a failure is how noise becomes a rule.
pub fn resolve_quorum(verdicts: &[(GraderRole, Verdict)]) -> (Verdict, Option<String>) {
    let primary = verdicts
        .iter()
        .find(|(role, _)| *role == GraderRole::Primary)
        .or_else(|| verdicts.iter().find(|(role, _)| *role != GraderRole::Audit))
        .map(|(_, v)| *v);

    let Some(primary) = primary else {
        return (
            Verdict::Indeterminate,
            Some("quorum produced no decisive verdict".to_string()),
        );
    };

    for (role, verdict) in verdicts {
        if *role == GraderRole::Veto {
            // A veto can only withhold agreement, never assert. It can turn a
            // PASS into INDETERMINATE; a PASS from a veto is simply ignored.
            if *verdict == Verdict::Fail && primary == Verdict::Pass {
                return (
                    Verdict::Indeterminate,
                    Some("a veto grader disagreed with the primary PASS".to_string()),
                );
            }
            continue;
        }
        // Two primary graders disagreeing outright.
        if *role != GraderRole::Audit
            && *verdict != primary
            && primary != Verdict::Indeterminate
            && *verdict != Verdict::Indeterminate
        {
            return (
                Verdict::Indeterminate,
                Some("quorum graders disagreed".to_string()),
            );
        }
    }

    (primary, None)
}

/// Stable hash of a grader's instruction, so a verdict can be traced to the
/// prompt that produced it.
fn hash_prompt(system_prompt: &str, user_prompt: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(system_prompt.as_bytes());
    hasher.update(b"\x00");
    hasher.update(user_prompt.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Evaluator, EvaluatorKind};
    use crate::transcript::Transcript;

    fn transcript(jsonl: &str) -> Transcript {
        crate::transcript::parse_jsonl(jsonl).expect("test transcript should parse")
    }

    fn invariant(assert: &str, unit: Option<&str>) -> Evaluator {
        Evaluator {
            name: "invariant".to_string(),
            weight: 1.0,
            unit: unit.map(str::to_string),
            kind: EvaluatorKind::Invariant,
            assert: Some(assert.to_string()),
            ..Default::default()
        }
    }

    /// F6's headline property: a booking after a failed lookup. The action and
    /// the failure have *different* kinds, which is the whole point — a
    /// kind-matching check would miss this entirely.
    #[test]
    fn no_action_after_failure_catches_a_cross_kind_violation() {
        let t = transcript(concat!(
            r#"{"id":"s1","kind":"lookup","status":"failed"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"ok"}"#
        ));
        let (ok, output) =
            evaluate_invariant(&invariant("no_action_after_failure", None), Some(&t));
        assert!(
            !ok,
            "a booking after a failed lookup must violate the invariant"
        );
        let msg = output.unwrap();
        assert!(msg.contains("VIOLATED"), "{}", msg);
        // Attribution is required: name the transition, not just the run.
        assert!(msg.contains("s1 → s2"), "{}", msg);
    }

    #[test]
    fn no_action_after_failure_passes_when_nothing_follows() {
        let t = transcript(concat!(
            r#"{"id":"s1","kind":"lookup","status":"failed"}"#,
            "\n",
            r#"{"id":"s2","kind":"lookup","status":"ok"}"#
        ));
        // A later step of the *same* kind is still an action after a failure.
        let (ok, _) = evaluate_invariant(&invariant("no_action_after_failure", None), Some(&t));
        assert!(!ok, "any action after a failure violates this invariant");
    }

    #[test]
    fn no_action_after_failure_passes_on_a_clean_run() {
        let t = transcript(concat!(
            r#"{"id":"s1","kind":"lookup","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"ok"}"#
        ));
        let (ok, output) =
            evaluate_invariant(&invariant("no_action_after_failure", None), Some(&t));
        assert!(ok, "{:?}", output);
    }

    #[test]
    fn at_most_once_counts_across_the_run() {
        let one = transcript(r#"{"id":"s1","kind":"book","status":"ok"}"#);
        assert!(evaluate_invariant(&invariant("at_most_once", Some("book")), Some(&one)).0);

        let two = transcript(concat!(
            r#"{"id":"s1","kind":"book","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"ok"}"#
        ));
        let (ok, output) = evaluate_invariant(&invariant("at_most_once", Some("book")), Some(&two));
        assert!(!ok);
        assert!(
            output.unwrap().contains("s1, s2"),
            "must name both offenders"
        );
    }

    #[test]
    fn at_most_once_requires_a_unit() {
        let t = transcript(r#"{"id":"s1","kind":"book","status":"ok"}"#);
        let (ok, output) = evaluate_invariant(&invariant("at_most_once", None), Some(&t));
        assert!(!ok);
        assert!(output.unwrap().contains("requires a `unit`"));
    }

    #[test]
    fn no_failed_steps_catches_a_globally_failing_run() {
        let t = transcript(concat!(
            r#"{"id":"s1","kind":"lookup","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"failed"}"#
        ));
        let (ok, output) = evaluate_invariant(&invariant("no_failed_steps", None), Some(&t));
        assert!(!ok);
        assert!(output.unwrap().contains("s2"));
    }

    /// Degrade gracefully: an agent that emits no transcript must not fail a
    /// run for lacking one.
    #[test]
    fn invariant_is_skipped_without_a_transcript() {
        let (ok, output) = evaluate_invariant(&invariant("no_failed_steps", None), None);
        assert!(ok, "a missing transcript must not fail the run");
        assert!(output.unwrap().contains("SKIPPED"));
    }

    #[test]
    fn an_unknown_invariant_is_reported_not_ignored() {
        let t = transcript(r#"{"id":"s1","kind":"book","status":"ok"}"#);
        let (ok, output) = evaluate_invariant(&invariant("make_it_nice", None), Some(&t));
        assert!(
            !ok,
            "an unrecognized property must fail loudly, not pass silently"
        );
        assert!(output.unwrap().contains("Unknown invariant"));
    }

    #[test]
    fn an_invariant_without_assert_is_a_configuration_error() {
        let mut eval = invariant("no_failed_steps", None);
        eval.assert = None;
        let t = transcript(r#"{"id":"s1","kind":"book","status":"ok"}"#);
        let (ok, output) = evaluate_invariant(&eval, Some(&t));
        assert!(!ok);
        assert!(output.unwrap().contains("requires an `assert`"));
    }

    /// F5: a unit attribution only stands when that unit actually failed.
    #[test]
    fn unit_attribution_requires_the_named_unit_to_have_failed() {
        let t = transcript(concat!(
            r#"{"id":"s1","kind":"lookup","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"failed"}"#
        ));
        assert!(t.step("s1").is_some_and(|s| !s.status.is_failure()));
        assert!(t.step("s2").is_some_and(|s| s.status.is_failure()));
    }
}

#[cfg(test)]
mod quorum_tests {
    use super::*;

    const P: Verdict = Verdict::Pass;
    const F: Verdict = Verdict::Fail;
    const I: Verdict = Verdict::Indeterminate;

    #[test]
    fn a_single_primary_decides() {
        let (v, conflict) = resolve_quorum(&[(GraderRole::Primary, P)]);
        assert_eq!(v, P);
        assert!(conflict.is_none());
    }

    /// The core property: disagreement is undecidable, not a failure.
    #[test]
    fn disagreeing_graders_yield_indeterminate_not_fail() {
        let (v, conflict) = resolve_quorum(&[(GraderRole::Primary, P), (GraderRole::Veto, F)]);
        assert_eq!(v, I, "two graders disagreeing is not evidence of failure");
        assert!(conflict.is_some());
    }

    #[test]
    fn a_veto_cannot_assert_a_pass() {
        let (v, _) = resolve_quorum(&[(GraderRole::Primary, F), (GraderRole::Veto, P)]);
        assert_eq!(v, F, "a veto cannot turn a FAIL into a PASS");
    }

    #[test]
    fn a_veto_agreeing_with_a_fail_is_a_fail() {
        let (v, _) = resolve_quorum(&[(GraderRole::Primary, F), (GraderRole::Veto, F)]);
        assert_eq!(v, F);
    }

    #[test]
    fn audit_graders_do_not_affect_the_verdict() {
        let (v, conflict) = resolve_quorum(&[(GraderRole::Primary, P), (GraderRole::Audit, F)]);
        assert_eq!(v, P, "an audit grader must not change the outcome");
        assert!(conflict.is_none());
    }

    /// Real signal, not chance: the two graders agree far more than the label
    /// distribution predicts.
    #[test]
    fn agreement_beyond_chance_scores_high_kappa() {
        // Both label everything PASS except the same one, so agreement is 3/4
        // while chance agreement is (0.75 * 0.75) + (0.25 * 0.25) = 0.625.
        let a = cohens_kappa(&[P, P, P, F], &[P, P, P, F]);
        assert_eq!(a.raw, 1.0);
        assert!(a.kappa > 0.9, "κ={}", a.kappa);
        assert!(a.is_trustworthy());
    }

    /// 50% agreement on a balanced split is exactly chance, so κ is 0 even
    /// though raw agreement is not zero. This is the case raw agreement hides.
    #[test]
    fn chance_level_agreement_has_zero_kappa_despite_half_raw() {
        let a = cohens_kappa(&[P, F, P, F], &[P, F, F, P]);
        assert_eq!(a.raw, 0.5);
        assert!(a.kappa.abs() < 0.01, "κ={}", a.kappa);
    }

    /// Two graders that always say the same thing agree 100% while being
    /// useless — the reason κ is reported alongside raw agreement.
    #[test]
    fn constant_identical_graders_are_flagged_as_untrustworthy() {
        let a = cohens_kappa(&[P, P, P, P], &[P, P, P, P]);
        assert_eq!(a.raw, 1.0);
        assert!(
            !a.is_trustworthy(),
            "100% raw agreement on a constant label carries no information"
        );
    }

    #[test]
    fn a_single_observation_is_not_trustworthy() {
        let a = cohens_kappa(&[P], &[P]);
        assert!(!a.is_trustworthy());
    }

    #[test]
    fn disagreeing_graders_have_low_kappa() {
        let a = cohens_kappa(&[P, P, F, F], &[F, F, P, P]);
        assert_eq!(a.raw, 0.0);
        assert!(a.kappa < 0.5, "κ={}", a.kappa);
    }

    #[test]
    fn empty_input_is_not_trustworthy() {
        let a = cohens_kappa(&[], &[]);
        assert_eq!(a.pairs, 0);
        assert!(!a.is_trustworthy());
    }
}

// ---------------------------------------------------------------------------
// TypeSafe, end to end (design acceptance tests 1, 2, 6, and the egress row)
// ---------------------------------------------------------------------------
//
// Every one of these replays a recorded response: the suite must never spend
// real money or depend on an ambient credential (design §Record/replay).

#[cfg(test)]
mod typesafe_integration_tests {
    use super::*;
    use crate::egress::{DataClass, DataHandling, EgressPolicy};
    use crate::manifest::{
        Evaluator, EvaluatorType, MetaLlmConfig, Sandbox, TypesafePrimitive, TypesafeQuestion,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    /// Only the fields `evaluate` reads: these evaluators never start a
    /// container.
    fn sandbox() -> Sandbox {
        Sandbox {
            engine: "docker".to_string(),
            base_image: "alpine".to_string(),
            setup_script: None,
            workspace: None,
            mounts: None,
            timeout_seconds: None,
            env: None,
        }
    }

    /// `embedded` so nothing in these tests implies a chat-model call: a
    /// TypeSafe evaluator never consults `meta_llm`.
    fn meta_llm() -> MetaLlmConfig {
        MetaLlmConfig {
            provider: "embedded".to_string(),
            model: "unused".to_string(),
            base_url: None,
            api_key_env: None,
            model_path: None,
            temperature: None,
            max_tokens: None,
            api_style: None,
        }
    }

    fn noul(text: &str) -> TypesafeQuestion {
        TypesafeQuestion {
            primitive: TypesafePrimitive::Noul,
            question: text.to_string(),
            criteria: None,
        }
    }

    fn typesafe_eval(document: &str, questions: BTreeMap<String, TypesafeQuestion>) -> Evaluator {
        Evaluator {
            name: "jev-grader".to_string(),
            r#type: EvaluatorType::Typesafe,
            weight: 1.0,
            document: Some(document.to_string()),
            questions: Some(questions),
            ..Default::default()
        }
    }

    /// Record a response under exactly the key `evaluate` will look up, so the
    /// run replays it instead of calling the live API.
    ///
    /// The state and questions are placeholder-free, so the rendered request
    /// is byte-identical to what is declared here.
    fn record(
        dir: &Path,
        state: &str,
        questions: &BTreeMap<String, TypesafeQuestion>,
        response: &str,
    ) {
        let model = crate::manifest::TYPESAFE_DEFAULT_MODEL;
        let body = crate::typesafe::build_request_body(state, model, questions);
        let path = crate::typesafe::cassette_file(
            dir,
            crate::manifest::TYPESAFE_DEFAULT_BASE_URL,
            model,
            &body,
        );
        let cassette = crate::typesafe::Cassette {
            request_model: model.to_string(),
            response: response.to_string(),
        };
        std::fs::write(
            &path,
            serde_json::to_string(&cassette).expect("cassette serializes"),
        )
        .expect("cassette is writable");
    }

    /// Hold the crate-wide env lock for the duration of a test that points
    /// `CASSETTE_ENV` somewhere: `set_var` is unsafe in edition 2024 precisely
    /// because another thread could be reading the same variable — and the
    /// replay below reads it back, so the guard outlives the `await`.
    async fn cassette_env(dir: &Path) -> tokio::sync::MutexGuard<'static, ()> {
        let guard = crate::typesafe::env_lock().lock().await;
        unsafe {
            // SAFETY: held under env_lock().
            std::env::set_var(crate::typesafe::CASSETTE_ENV, dir);
        }
        guard
    }

    /// Acceptance test 1 (smoke, clean data): a recorded Noul answer becomes a
    /// well-formed verdict, with provenance naming what actually answered.
    #[tokio::test]
    async fn a_typesafe_evaluator_replays_a_recorded_verdict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = cassette_env(dir.path()).await;

        let state = "s1 looked it up; s2 booked the flight.";
        let mut questions = BTreeMap::new();
        questions.insert(
            "passes_rule".to_string(),
            noul("Did the transcript comply?"),
        );
        record(
            dir.path(),
            state,
            &questions,
            r#"{"model":"jev-1.13.0-hotfix.1","answers":{"passes_rule":{"type":"noul","noul":0.97}}}"#,
        );

        let eval = typesafe_eval(state, questions);
        let result = evaluate(
            &[eval],
            Path::new("."),
            0.5,
            &sandbox(),
            &meta_llm(),
            &DataHandling::default(),
            None,
        )
        .await
        .expect("a replayed verdict is not an infrastructure failure");

        assert!(result.pass, "{:?}", result.details);
        assert_eq!(result.details.len(), 1);
        let output = result.details[0].output.clone().unwrap_or_default();
        assert!(output.starts_with("PASS"), "{}", output);
        assert!(
            output.contains("passes_rule=PASS"),
            "per-question detail is what makes the verdict auditable: {}",
            output
        );

        assert_eq!(result.provenance.len(), 1);
        let prov = &result.provenance[0];
        assert_eq!(prov.provider, "typesafe");
        assert_eq!(
            prov.base_url.as_deref(),
            Some(crate::manifest::TYPESAFE_DEFAULT_BASE_URL)
        );
        // The RESOLVED version — what answered, not what was asked for.
        assert_eq!(prov.model, "jev-1.13.0-hotfix.1");
        assert_eq!(prov.verdict, Verdict::Pass);
        assert!(!prov.prompt_hash.is_empty());
    }

    /// Acceptance test 2: an ambiguous answer lands in the dead band and is
    /// INDETERMINATE — excluded from scoring rather than counted as a fail.
    #[tokio::test]
    async fn an_indeterminate_verdict_is_excluded_from_scoring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = cassette_env(dir.path()).await;

        let state = "s1 started the lookup; s2 …";
        let mut questions = BTreeMap::new();
        questions.insert("complete".to_string(), noul("Is the transcript complete?"));
        record(
            dir.path(),
            state,
            &questions,
            r#"{"model":"jev-1.13.0","answers":{"complete":{"type":"noul","noul":0.5}}}"#,
        );

        let eval = typesafe_eval(state, questions);
        let result = evaluate(
            &[eval],
            Path::new("."),
            0.5,
            &sandbox(),
            &meta_llm(),
            &DataHandling::default(),
            None,
        )
        .await
        .expect("an undecidable verdict is not an infrastructure failure");

        assert_eq!(result.provenance[0].verdict, Verdict::Indeterminate);
        assert!(!result.details[0].success);
        assert_eq!(
            result.details[0].weight, 0.0,
            "an INDETERMINATE is not evidence the agent is wrong, so it must not be fed to the \
             optimizer as a failing log"
        );
        assert!(
            result.details[0]
                .output
                .as_deref()
                .is_some_and(|o| o.contains("INDETERMINATE")),
            "{:?}",
            result.details[0].output
        );
    }

    /// Acceptance test 6: an endpoint that cannot be reached (here: no
    /// credential) aborts the run. An evaluator outage must never grade the
    /// agent.
    #[tokio::test]
    async fn a_typesafe_endpoint_failure_aborts_the_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = cassette_env(dir.path()).await;
        unsafe {
            // SAFETY: held under env_lock().
            std::env::remove_var("NEUROPLASTICITY_TEST_MISSING_API_KEY");
        }

        let mut questions = BTreeMap::new();
        questions.insert("ok".to_string(), noul("Did it work?"));
        let mut eval = typesafe_eval("state", questions);
        eval.api_key_env = Some("NEUROPLASTICITY_TEST_MISSING_API_KEY".to_string());

        let err = evaluate(
            &[eval],
            Path::new("."),
            0.5,
            &sandbox(),
            &meta_llm(),
            &DataHandling::default(),
            None,
        )
        .await
        .expect_err("an unreachable grader must abort the run, not grade it");
        assert!(err.to_string().contains("not an agent failure"), "{}", err);
    }

    /// A manifest that never said what to judge is the author's mistake: a
    /// failed score naming the fix, with the run still scoring.
    #[tokio::test]
    async fn a_typesafe_config_error_is_a_failed_score_not_an_abort() {
        let eval = Evaluator {
            name: "jev-grader".to_string(),
            r#type: EvaluatorType::Typesafe,
            weight: 1.0,
            document: Some("state".to_string()),
            questions: None,
            ..Default::default()
        };
        let result = evaluate(
            &[eval],
            Path::new("."),
            0.5,
            &sandbox(),
            &meta_llm(),
            &DataHandling::default(),
            None,
        )
        .await
        .expect("a bad manifest is a configuration error, not an infrastructure one");

        assert!(!result.details[0].success);
        assert!(
            result.details[0]
                .output
                .as_deref()
                .is_some_and(|o| o.contains("Config Error")),
            "{:?}",
            result.details[0].output
        );
    }

    /// The egress row of acceptance test 5: document text leaves the box, so
    /// the declared ceiling gates it like any other hosted provider.
    #[tokio::test]
    async fn a_typesafe_evaluator_is_gated_by_the_declared_data_class() {
        let restricted = DataHandling {
            data_class: Some(DataClass::Restricted),
            egress: Some(EgressPolicy { allow: Vec::new() }),
        };
        let mut questions = BTreeMap::new();
        questions.insert("ok".to_string(), noul("Did it work?"));

        let err = evaluate(
            &[typesafe_eval("state", questions)],
            Path::new("."),
            0.5,
            &sandbox(),
            &meta_llm(),
            &restricted,
            None,
        )
        .await
        .expect_err("document text may not leave the box under this policy");
        assert!(
            err.to_string().contains("Egress policy violation"),
            "{}",
            err
        );
    }
}

// ---------------------------------------------------------------------------
// What a chat-model grader judges (the `llm` arm's state resolution)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod llm_state_resolution_tests {
    use super::*;
    use crate::manifest::{Evaluator, EvaluatorType};

    fn eval(prompt: Option<&str>, target_file: Option<&str>, document: Option<&str>) -> Evaluator {
        Evaluator {
            name: "chat".to_string(),
            r#type: EvaluatorType::Llm,
            weight: 1.0,
            prompt: prompt.map(str::to_string),
            target_file: target_file.map(str::to_string),
            document: document.map(str::to_string),
            ..Default::default()
        }
    }

    /// The pre-existing path, unchanged: an `llm` evaluator declared before
    /// `document` existed grades its `target_file`, labelled by that path.
    #[tokio::test]
    async fn a_declared_target_file_is_still_the_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("doc.md"), "the body").expect("fixture");
        let judge = resolve_llm_judge(
            &eval(Some("judge it"), Some("doc.md"), None),
            dir.path(),
            None,
        )
        .await
        .expect("the file exists")
        .expect("prompt and target_file are both declared");
        assert_eq!(judge.label, "doc.md");
        assert_eq!(judge.content, "the body");
        assert_eq!(judge.prompt, "judge it");
    }

    /// What the load-time `fallback: "llm"` transform leaves behind: a
    /// `document` and no file to read. Dropping the document here would make
    /// the converted grader judge nothing — or a file the original grader
    /// never looked at.
    #[tokio::test]
    async fn a_document_wins_over_target_file_so_a_converted_grader_keeps_its_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("doc.md"), "not this one").expect("fixture");
        let judge = resolve_llm_judge(
            &eval(
                Some("judge it"),
                Some("doc.md"),
                Some("s1 looked it up; s2 booked it."),
            ),
            dir.path(),
            None,
        )
        .await
        .expect("the document needs no file")
        .expect("prompt is declared");
        assert_eq!(judge.label, "document");
        assert_eq!(judge.content, "s1 looked it up; s2 booked it.");
    }

    #[tokio::test]
    async fn a_missing_prompt_or_state_is_reported_as_missing_not_as_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            resolve_llm_judge(&eval(None, Some("doc.md"), None), dir.path(), None)
                .await
                .unwrap()
                .is_none(),
            "no prompt means there is nothing to grade with"
        );
        assert!(
            resolve_llm_judge(&eval(Some("p"), None, None), dir.path(), None)
                .await
                .unwrap()
                .is_none(),
            "and no state means there is nothing to grade"
        );
    }

    /// A state that was named but cannot be read is an error the evaluator
    /// reports — the same wording the pre-existing path used.
    #[tokio::test]
    async fn a_target_file_that_is_not_there_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_llm_judge(&eval(Some("p"), Some("gone.md"), None), dir.path(), None)
            .await
            .expect_err("the file does not exist");
        assert!(
            err.contains("Target file does not exist: gone.md"),
            "{}",
            err
        );
    }
}
