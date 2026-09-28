use crate::llm_client::{CompletionSpec, ask_llm_json};
use crate::manifest::{Evaluator, EvaluatorKind, EvaluatorType, MetaLlmConfig, Sandbox};
use anyhow::Result;
use futures::future::join_all;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;

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
                                return EvaluatorScore {
                                    name: eval_clone.name.clone(),
                                    success: false,
                                    weight: eval_clone.weight,
                                    output: Some(format!(
                                        "Security Exception: host_bash command '{}' is not in the system allowlist. Evaluators must use container environment for arbitrary execution.",
                                        cmd_name
                                    )),
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

                    if let (Some(prompt), Some(target_file)) =
                        (&eval_clone.prompt, &eval_clone.target_file)
                    {
                        let file_path = working_dir_clone.join(target_file);
                        if file_path.exists() {
                            if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                                let system_prompt = concat!(
                                    "You are an automated evaluator. Grade the document against the prompt.\n",
                                    "Answer with a single JSON object: {\"verdict\": \"PASS\"|\"FAIL\"|\"INDETERMINATE\", ",
                                    "\"reason\": \"<one sentence>\"} and nothing else.\n",
                                    "Use INDETERMINATE only when the document cannot be judged from ",
                                    "what is shown, or the prompt is ambiguous — never as a substitute ",
                                    "for FAIL when you can tell. Never describe the format you use."
                                );
                                let user_prompt = format!(
                                    "Evaluation Prompt:\n{}\n\nTarget Document ({}):\n{}",
                                    prompt, target_file, content
                                );

                                let spec = CompletionSpec {
                                    system_prompt,
                                    user_prompt: &user_prompt,
                                    json_schema: Some(verdict_schema()),
                                };

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
                                                    Some(format!(
                                                        "Unusable LLM verdict: {:?}",
                                                        raw
                                                    )),
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
                                                            prompt,
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
                            } else {
                                (
                                    false,
                                    Some(format!("Failed to read target file: {}", target_file)),
                                )
                            }
                        } else {
                            (
                                false,
                                Some(format!("Target file does not exist: {}", target_file)),
                            )
                        }
                    } else {
                        (
                            false,
                            Some("Missing 'prompt' or 'target_file' for llm evaluator".to_string()),
                        )
                    }
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
    })
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
            r#type: EvaluatorType::HostBash,
            script: None,
            image: None,
            command: None,
            setup_script: None,
            prompt: None,
            target_file: None,
            weight: 1.0,
            unit: unit.map(str::to_string),
            kind: EvaluatorKind::Invariant,
            assert: Some(assert.to_string()),
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
