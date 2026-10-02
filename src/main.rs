use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::rules::Rule;

pub mod container;
pub mod egress;
#[cfg(feature = "embedded-llm")]
pub mod embedded_llm;
pub mod evaluator;
pub mod fingerprint;
pub mod llm_client;
pub mod manifest;
pub mod optimizer;
pub mod patch;
pub mod reporter;
pub mod rules;
pub mod runner;
pub mod transcript;

/// Result of running one manifest: pass/fail, epochs used, and the per-evaluator
/// outcomes needed for a baseline diff.
struct ManifestRun {
    passed: bool,
    epochs_taken: u32,
    manifest: manifest::PlasticityManifest,
    report: patch::ManifestReport,
    /// True when the policy is `block` and a regression was detected.
    blocked_by_regression: bool,
    /// Protected-rule changes that were reverted (F2).
    quarantined: Vec<rules::QuarantinedChange>,
    /// Verdict provenance accumulated across epochs (F4a).
    provenance: Vec<evaluator::VerdictProvenance>,
    /// The run's ordered transcript, when the agent emitted one (F6a).
    transcript: Option<transcript::Transcript>,
    /// Which unit each generated rule addresses (F5).
    rule_units: Vec<Option<String>>,
    /// Digests of the inputs this run was derived from (F8).
    patch_provenance: fingerprint::PatchProvenance,
    /// Grader agreement, when a quorum was used (F4b).
    agreement: Vec<evaluator::Agreement>,
    /// Set when the run stopped on a budget limit (F7).
    budget_halt: Option<String>,
}

async fn run_single_manifest(manifest_path: &Path) -> Result<ManifestRun> {
    let manifest_content = fs::read_to_string(manifest_path)
        .with_context(|| format!("Failed to read {:?}", manifest_path))?;
    let manifest: manifest::PlasticityManifest = serde_json::from_str(&manifest_content)
        .with_context(|| format!("Failed to parse {:?}", manifest_path))?;

    let run_id = Uuid::new_v4().to_string();
    println!("Starting Run ID: {}", run_id);
    println!("Target Project: {}", manifest.name);

    let max_epochs = manifest.optimization.epochs;
    let pass_threshold = manifest.optimization.pass_threshold;
    let base_image = &manifest.sandbox.base_image;
    let agent_command = &manifest.agent_command;

    let target_rules_file = PathBuf::from(&manifest.optimization.target_rules_file);

    // Egress policy (F3). Opt-in: a manifest with no `data_class` is
    // unclassified and every check below is a no-op.
    let data_handling = manifest
        .optimization
        .data
        .as_ref()
        .map(|d| egress::DataHandling {
            data_class: d.data_class,
            egress: d.egress.clone(),
        })
        .unwrap_or_default();
    let meta_llm_provider = manifest.optimization.meta_llm.provider.clone();

    // Warn when no hosted provider is allowed. The embedded provider is always
    // permitted (it never leaves the machine), so an embedded-only manifest with
    // an empty allow list is a legitimate setup, not a misconfiguration.
    let hosted_permitted = data_handling
        .egress
        .as_ref()
        .is_some_and(|policy| policy.ceiling(egress::EgressProvider::Hosted).is_some());
    if data_handling.data_class.is_some() && !hosted_permitted {
        println!(
            "ℹ️  data_class is `{}` and no hosted provider is allowed, so the Meta-Optimizer \
             will be limited to local (`embedded`) inference. Add an `egress.allow` entry if \
             you intend to use a remote model.",
            data_handling
                .data_class
                .map(|c| c.label())
                .unwrap_or("unset")
        );
    }

    // F8: digests of the inputs this run is derived from, so a later
    // re-verification can tell whether the patch still applies.
    let baseline_rules_digest = match fs::read_to_string(&target_rules_file) {
        Ok(existing) => fingerprint::digest_of(&[&existing]),
        Err(_) => fingerprint::digest_of(&[""]),
    };
    let evaluators_json_for_hash = serde_json::to_string(&manifest.evaluators).unwrap_or_default();
    let patch_provenance = fingerprint::PatchProvenance {
        manifest_hash: fingerprint::digest_of(&[&manifest_content]),
        evaluator_set_hash: fingerprint::digest_of(&[&evaluators_json_for_hash]),
        baseline_rules_digest,
        result_rules_digest: String::new(),
        transcript_digest: None,
        target_project: manifest.name.clone(),
        run_started_at: fingerprint::now_iso8601(),
        run_finished_at: String::new(),
    };

    // Checked before any container starts (F3: fail loud, early).
    egress::enforce_egress(
        &data_handling,
        &meta_llm_provider,
        egress::EgressKind::Optimizer,
    )
    .context("The Meta-Optimizer's provider is not permitted for this manifest's data class")?;

    // Prevent Path Traversal (P0 Fix)
    if target_rules_file.is_absolute()
        || target_rules_file
            .components()
            .any(|c| c.as_os_str() == "..")
    {
        anyhow::bail!(
            "Security Exception: target_rules_file must be a safe, relative path inside the project directory."
        );
    }

    // Budget (F7). No cap unless the manifest declares one.
    let mut tracker =
        manifest::BudgetTracker::new(manifest.optimization.budget.clone().unwrap_or_default());
    if tracker.is_active() {
        println!(
            "💰 Budget active ({}).",
            match manifest.optimization.budget.as_ref().map(|b| b.on_exceed) {
                Some(manifest::OnExceed::Halt) => "halts on exceed".to_string(),
                _ => "warns on exceed".to_string(),
            }
        );
    }

    // A spend cap that cannot be computed is worse than none: it looks enforced
    // and is not. Say so rather than letting a budget silently not apply.
    if manifest
        .optimization
        .budget
        .as_ref()
        .is_some_and(manifest::Budget::spend_cap_is_inert)
    {
        println!(
            "⚠️  `max_usd` is set but `cost_per_1k_input_usd` / `cost_per_1k_output_usd` are \
             not, so the spend cap CANNOT be enforced and will never trigger. Configure both \
             prices, or remove `max_usd`."
        );
    }

    // Baseline is captured on the first epoch that actually evaluates, before
    // any rule has been mutated, and never re-captured.
    let mut baseline: Option<Vec<patch::EvaluatorOutcome>> = None;
    let mut final_results: Vec<patch::EvaluatorOutcome> = Vec::new();
    // Protected-rule changes reverted along the way (F2).
    let mut quarantined: Vec<rules::QuarantinedChange> = Vec::new();
    // Verdict provenance accumulated across epochs (F4a).
    let mut provenance: Vec<evaluator::VerdictProvenance> = Vec::new();
    // Set when the run stops on a budget limit (F7).
    let mut budget_halt: Option<String> = None;
    // Ordered transcript, when the agent emitted one (F6a).
    let mut transcript: Option<transcript::Transcript> = None;
    // The transcript step a failing evaluator attributed this epoch to (F5).
    let mut failing_unit: Option<String> = None;
    // Which unit each generated rule addresses, aligned with rule order.
    let mut rule_units: Vec<Option<String>> = Vec::new();
    // Grader agreement figures, when a quorum was used (F4b).
    let mut agreement: Vec<evaluator::Agreement> = Vec::new();

    // Counts epochs actually run, so a budget halt does not report the
    // configured maximum as if it had been reached.
    let mut epochs_run: u32 = 0;

    for epoch in 1..=max_epochs {
        println!("\n--- Epoch {} / {} ---", epoch, max_epochs);

        // Checked before doing any more work, so a halted run stops promptly
        // rather than after the next container spin-up (F7).
        if let Some(reason) = tracker.check() {
            println!("🛑 Budget exceeded: {}", reason);
            budget_halt = Some(reason);
            break;
        }
        epochs_run = epoch as u32;

        // Calculate run fingerprint
        let evaluators_json = serde_json::to_string(&manifest.evaluators).unwrap_or_default();
        let sandbox_json = serde_json::to_string(&manifest.sandbox).unwrap_or_default();
        let fingerprint = fingerprint::calculate_fingerprint(
            agent_command,
            &target_rules_file,
            &manifest.name,
            &manifest.optimization.meta_llm.provider,
            &manifest.optimization.meta_llm.model,
            manifest
                .optimization
                .meta_llm
                .base_url
                .as_deref()
                .unwrap_or_default(),
            &evaluators_json,
            &sandbox_json,
        );

        // Declared before the fast-path branch below: either the cache supplies
        // these values or the sandbox run does.
        let stdout: String;
        let stderr: String;
        let score: f64;
        let pass: bool;

        if let Some(cached_failure) = fingerprint::check_fingerprint(&fingerprint) {
            println!(
                "⚡ FAST PATH: Known failure fingerprint ({}) detected for this exact rule configuration.",
                fingerprint
            );
            println!("Skipping 120s container execution and loading cached side-effects...");
            stdout = cached_failure.stdout;
            stderr = cached_failure.stderr;
            score = cached_failure.score;
            pass = false; // We only cache failures
        } else {
            // 2. Isolate: Setup scratch workspace
            println!("Setting up ephemeral workspace...");
            let scratch_dir = runner::setup_workspace(Path::new("."))
                .context("Failed to setup ephemeral workspace")?;
            let scratch_path = scratch_dir.path();

            // 3. Execute Agent in Podman
            println!("Executing agent in sandbox ({})...", base_image);
            let (sandbox_stdout, sandbox_stderr, _success) = runner::run_agent(
                Path::new("."),
                scratch_path,
                &manifest.sandbox,
                agent_command,
            )
            .await
            .context("Failed to run agent in container sandbox")?;

            stdout = sandbox_stdout;
            stderr = sandbox_stderr;
            println!("=== AGENT STDOUT ===\n{}\n=== END STDOUT ===", stdout);
            println!("=== AGENT STDERR ===\n{}\n=== END STDERR ===", stderr);

            // F6a: an agent may emit an ordered transcript alongside stdout.
            // Additive — a missing or unusable transcript never fails a run.
            transcript = transcript::read_sidecar(scratch_path);
            if let Some(t) = &transcript {
                println!(
                    "📋 Transcript: {} step(s), {} failed",
                    t.len(),
                    t.failing_steps().len()
                );
            }

            // 4. Evaluate & Score
            println!("Evaluating side effects...");
            let eval_result = evaluator::evaluate(
                &manifest.evaluators,
                scratch_path,
                pass_threshold,
                &manifest.sandbox,
                &manifest.optimization.meta_llm,
                &data_handling,
                transcript.as_ref(),
            )
            .await
            .context("Evaluator execution failed")?;

            score = eval_result.score;
            pass = eval_result.pass;

            // An INDETERMINATE grader verdict is not evidence the agent is wrong.
            // Passing it to the optimizer as a failing log is precisely how
            // noise becomes a rule, so it is withheld and the run is halted
            // rather than optimized against (F4a).
            let indeterminate: Vec<String> = eval_result
                .details
                .iter()
                .filter(|d| {
                    d.output
                        .as_deref()
                        .is_some_and(|o| o.starts_with("INDETERMINATE"))
                })
                .map(|d| d.name.clone())
                .collect();

            // F5: remember which unit a failing evaluator blamed, so the rule
            // can be scoped to it and the patch can say why.
            failing_unit = eval_result
                .details
                .iter()
                .find(|d| !d.success && d.attributed_unit.is_some())
                .and_then(|d| d.attributed_unit.clone());

            if !indeterminate.is_empty() {
                anyhow::bail!(
                    "Grader returned INDETERMINATE for {}. That is not evidence the agent \
                     failed, so the run is halted rather than optimized against a guess.\n\
                     Improve the grader prompt, or use a stronger `meta_llm` model for it.",
                    indeterminate.join(", ")
                );
            }

            // Record the baseline on the first evaluated epoch, and track the
            // final result set for the delta diff (F1).
            let outcomes = patch::EvaluatorOutcome::from_result(&eval_result);
            if baseline.is_none() {
                baseline = Some(outcomes.clone());
            }
            final_results = outcomes;
            provenance.extend(eval_result.provenance.iter().cloned());
            agreement.extend(eval_result.agreement.iter().copied());

            println!(
                "Score: {:.2} (Threshold: {:.2})",
                score, eval_result.threshold
            );

            // If it failed, save to fingerprint cache so we never run this exact configuration again
            if !pass {
                let _ = fingerprint::save_fingerprint(
                    &fingerprint,
                    fingerprint::CachedFailure {
                        score,
                        stdout: stdout.clone(),
                        stderr: stderr.clone(),
                    },
                );
            }

            println!("Cleaning up ephemeral workspace...");
        }

        // 5. Observe & Report
        //
        // Emitted for the fast path as well as a fresh run: a cached epoch must
        // still leave a report artifact, otherwise the run produces logs but no
        // evidence of what was scored.
        println!("Generating epoch report...");
        let reporter = reporter::Reporter::new();
        reporter
            .report_epoch(
                &run_id,
                epoch as u32,
                &stdout,
                &stderr,
                score,
                vec![], // We'll add mutations here if applicable
            )
            .context("Failed to write epoch report")?;

        if pass {
            println!("✅ Epoch {} achieved passing score! Run complete.", epoch);
            let result_rules_digest = fs::read_to_string(&target_rules_file)
                .map(|c| fingerprint::digest_of(&[&c]))
                .unwrap_or_else(|_| fingerprint::digest_of(&[""]));
            return Ok(ManifestRun {
                passed: true,
                epochs_taken: epoch as u32,
                blocked_by_regression: false,
                quarantined,
                provenance,
                agreement: agreement.clone(),
                patch_provenance: fingerprint::PatchProvenance {
                    result_rules_digest,
                    transcript_digest: transcript.as_ref().map(|t| t.digest()),
                    run_finished_at: fingerprint::now_iso8601(),
                    ..patch_provenance
                },
                transcript,
                rule_units,
                budget_halt,
                report: patch::ManifestReport {
                    manifest_name: manifest.name.clone(),
                    passed: true,
                    epochs_taken: epoch as u32,
                    baseline: baseline.clone().unwrap_or_default(),
                    final_results,
                },
                manifest,
            });
        }

        // 7. Optimize & Mutate
        if epoch < max_epochs {
            println!("❌ Score below threshold. Invoking Meta-Optimizer...");

            let effective_rule_policy = manifest
                .optimization
                .rules
                .as_ref()
                .map(|r| r.policy.clone())
                .unwrap_or_default();

            // Read existing rules to pass to the optimizer as context. Accepts
            // both bare strings and classified objects (F2).
            let existing_rules: Vec<Rule> = if target_rules_file.exists() {
                match fs::read_to_string(&target_rules_file) {
                    Ok(content) => rules::parse_rules(&content)
                        .with_context(|| format!("Failed to read {:?}", target_rules_file))?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                    Err(e) => {
                        return Err(anyhow::Error::new(e)
                            .context(format!("Failed to read {:?}", target_rules_file)));
                    }
                }
            } else {
                Vec::new()
            };

            // The optimizer is only shown behavioral rules. A protected
            // constraint is not something it is being asked to improve, and
            // showing it invites the model to reword it (F2).
            let optimizer_visible: Vec<Rule> = existing_rules
                .iter()
                .filter(|r| !effective_rule_policy.protects(r))
                .cloned()
                .collect();
            if optimizer_visible.len() != existing_rules.len() {
                println!(
                    "   🔒 {} protected rule(s) hidden from the optimizer.",
                    existing_rules.len() - optimizer_visible.len()
                );
            }

            // F5: when the failure is attributable to a step, lead the
            // optimizer with that narrow context so the rule can be scoped to
            // it. The whole-run log still follows as context.
            let localized = transcript
                .as_ref()
                .and_then(|t| t.render_localized_failure(failing_unit.as_deref()));
            if localized.is_some() {
                println!(
                    "   🎯 Failure localized to a transcript step; the rule will be scoped to it."
                );
            }

            let budget_cfg = manifest.optimization.budget.clone().unwrap_or_default();
            let spec = optimizer::optimizer_spec(
                &stderr,
                &manifest.task_prompt,
                &optimizer_visible,
                localized.as_deref(),
            );
            let (reply, usage) =
                llm_client::complete_tracked(&manifest.optimization.meta_llm, &spec.as_spec())
                    .await
                    .with_context(|| {
                        "The Meta-Optimizer could not produce a usable new rule. The agent is not \
                 necessarily still at fault — verify the meta_llm endpoint and key before \
                 trusting any further optimization."
                    })?;
            if let Some(usage) = usage {
                llm_client::report_spend(&mut tracker, &usage, &budget_cfg);
            }
            let new_rule = optimizer::finish_rule(reply, &optimizer_visible)?;

            // Append the generated rule to rules.json
            if let Some(parent) = target_rules_file.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut proposed = existing_rules.clone();
            proposed.push(Rule::behavioral(new_rule.text.clone()));
            // F5: which step motivated this rule, recorded for the patch.
            rule_units.push(new_rule.unit.clone());

            // Enforce the rule policy before writing (F2): a protected rule the
            // optimizer dropped or altered is restored, never persisted as lost.
            let quarantine =
                rules::enforce_policy(&existing_rules, &proposed, &effective_rule_policy);

            if !quarantine.is_empty() {
                println!(
                    "\n⚠️  {} protected rule(s) were proposed for change — reverted and quarantined:",
                    quarantine.changes.len()
                );
                for change in &quarantine.changes {
                    println!("     - \"{}\": {}", change.rule_text, change.reason);
                    println!(
                        "       This requires human sign-off. It is NOT reported as an improvement."
                    );
                }
                quarantined.extend(quarantine.changes.clone());
            }

            fs::write(
                &target_rules_file,
                rules::serialize_rules(&quarantine.accepted)?,
            )?;

            if !quarantine.is_empty() {
                // Keep the attempted change out of the rules file, but
                // auditable, so a reviewer can see what the optimizer wanted.
                let quarantine_path = Path::new("neuroplasticity_quarantine.md");
                let mut doc = String::from("# ⚠️ Quarantined rule changes\n\n");
                doc.push_str(
                    "The Meta-Optimizer attempted to change rules marked as protected. These \
                     changes were **reverted** and were not applied. Review each one manually:\n\n",
                );
                for (i, change) in quarantine.changes.iter().enumerate() {
                    doc.push_str(&format!(
                        "## {}\n- **Protected rule:** `{}`\n- **Reason:** {}\n\n",
                        i + 1,
                        change.rule_text,
                        change.reason
                    ));
                }
                if fs::write(quarantine_path, doc).is_ok() {
                    println!("   📄 Quarantine report written to {:?}", quarantine_path);
                }
            }

            println!("Applied new rule optimization to {:?}", target_rules_file);
        }
    }

    // A budget halt is the proximate cause, not "exhausted the epochs": report
    // the epochs actually run so a halted run is not described as if it had
    // reached a conclusion.
    let stopped_early = budget_halt.is_some();
    if stopped_early {
        println!(
            "❌ Stopped after {} of {} configured epoch(s) on budget.",
            epochs_run, max_epochs
        );
    } else {
        println!("❌ Max epochs reached without achieving pass threshold.");
    }
    let epochs_taken = if stopped_early {
        epochs_run
    } else {
        max_epochs as u32
    };

    let baseline_outcomes = baseline.unwrap_or_default();
    let report = patch::ManifestReport {
        manifest_name: manifest.name.clone(),
        passed: false,
        epochs_taken,
        baseline: baseline_outcomes,
        final_results,
    };
    // `block` refuses to present a rule set that regressed something green.
    let blocked_by_regression = manifest
        .optimization
        .regression_guard
        .as_ref()
        .map(|g| g.policy == manifest::RegressionPolicy::Block)
        .unwrap_or(false)
        && !report.regressions().is_empty();

    let result_rules_digest = fs::read_to_string(&target_rules_file)
        .map(|c| fingerprint::digest_of(&[&c]))
        .unwrap_or_else(|_| fingerprint::digest_of(&[""]));
    let patch_provenance = fingerprint::PatchProvenance {
        result_rules_digest,
        transcript_digest: transcript.as_ref().map(|t| t.digest()),
        run_finished_at: fingerprint::now_iso8601(),
        ..patch_provenance
    };

    Ok(ManifestRun {
        passed: false,
        epochs_taken,
        blocked_by_regression,
        quarantined,
        provenance,
        transcript,
        rule_units,
        patch_provenance,
        agreement,
        budget_halt,
        report,
        manifest,
    })
}

/// Re-verify that a patch still applies to its target (F8).
///
/// A patch is prose rules. If the target's prompt has drifted since the run,
/// re-applying it can reintroduce a fix that is now wrong — and re-running the
/// evaluators would report a result for a different artifact than the patch
/// describes. So the digest is checked first and drift is reported rather than
/// silently verified. The check is skippable only with an explicit flag.
fn verify_patch(args: &[String], pos: usize) -> Result<()> {
    let allow_drift = args.iter().any(|a| a == "--allow-drift");
    let patch_path = args
        .get(pos + 1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("neuroplasticity_patch.md"));
    let manifest_path = args
        .get(pos + 2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("plasticity.json"));

    if !patch_path.exists() {
        anyhow::bail!("No patch artifact at {:?}.", patch_path);
    }
    let patch = fs::read_to_string(&patch_path)
        .with_context(|| format!("Failed to read {:?}", patch_path))?;

    let Some(expected) = extract_digest(&patch, "Baseline target rules digest") else {
        anyhow::bail!(
            "{:?} carries no baseline rules digest, so it cannot be verified. \
             Patches produced before provenance was recorded have nothing to compare against.",
            patch_path
        );
    };
    let manifest_hash = extract_digest(&patch, "Manifest hash");
    let evaluator_hash = extract_digest(&patch, "Evaluator set hash");
    let finished_at = extract_digest(&patch, "Finished at");

    println!("Patch:  {:?}", patch_path);
    println!(
        "Run:    {} · finished {}",
        manifest_hash.as_deref().unwrap_or("?"),
        finished_at.as_deref().unwrap_or("?")
    );
    println!("Expected rules digest: {}\n", expected);

    // The manifest is optional for a pure digest comparison.
    let mut manifest_matches = None;
    if manifest_path.exists() {
        let content = fs::read_to_string(&manifest_path)?;
        let current = fingerprint::digest_of(&[&content]);
        manifest_matches = Some(current.clone());
        println!("Manifest digest:      {}", current);
    }

    let current_rules = if manifest_path.exists() {
        let content = fs::read_to_string(&manifest_path)?;
        let parsed: manifest::PlasticityManifest = serde_json::from_str(&content)?;
        let rules_path = PathBuf::from(&parsed.optimization.target_rules_file);
        fs::read_to_string(&rules_path).ok()
    } else {
        None
    };

    let drift = fingerprint::check_rules_drift(&expected, current_rules.as_deref());

    if let (Some(expected_manifest), Some(actual_manifest)) =
        (manifest_hash.as_deref(), manifest_matches.as_deref())
    {
        if expected_manifest != actual_manifest {
            println!("\n⚠️  The manifest itself has changed since this patch was generated.");
        }
    }
    let _ = evaluator_hash;

    if drift.is_match() {
        println!("\n✅ Rules digest matches. This patch still applies to the current target.");
        return Ok(());
    }

    match drift {
        fingerprint::DigestDrift::Drifted { actual, .. } => {
            println!("\nCurrent rules digest:  {}", actual);
            if allow_drift {
                println!(
                    "\n⚠️  DRIFT DETECTED, but --allow-drift was passed.\n\
                     Re-running now would verify a different artifact than this patch describes."
                );
                return Ok(());
            }
            println!(
                "\n🛑 DRIFT DETECTED — refusing to verify.\n\
                 The target's rules have changed since this patch was generated, so a re-run would\n\
                 describe a different prompt than the one these rules were derived from. Re-applying\n\
                 the patch could reintroduce a fix that is now wrong.\n\
                 Review the drift, then pass --allow-drift to verify anyway."
            );
            std::process::exit(3);
        }
        fingerprint::DigestDrift::Match => unreachable!(),
    }
}

/// Pull a `- **Label:** \`value\`` field out of a patch header.
///
/// The separator is `:** ` — a plain ": " does not appear, because the value
/// itself may contain colons (a `sha256:` digest).
fn extract_digest(patch: &str, label: &str) -> Option<String> {
    patch.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("- **")?;
        let (key, value) = rest.split_once(":** ")?;
        let key = key.trim();
        if key != label {
            return None;
        }
        let value = value.trim().trim_matches('`').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== NeuroPlasticity Orchestrator ===");

    // 1. Parse & Validate Manifest
    let mut args = std::env::args();
    args.next(); // Skip executable name

    let mut manifest_path_str = "plasticity.json".to_string();
    // Collected because std::env::Args is not Clone and the flags are read twice.
    let raw_args: Vec<String> = args.by_ref().collect();

    // HELP GATE (first-run defect 2026-10-01): `--help` (and `-h`) previously
    // fell through every branch below, defaulted the manifest path to
    // plasticity.json, and STARTED A REAL RUN. Asking for help must never
    // execute anything.
    if raw_args.iter().any(|a| a == "--help" || a == "-h") {
        println!("Usage: NeuroPlasticity [test <manifest.json>] [--print-egress-plan] | verify-patch ...");
        println!("  (no args)      runs ./plasticity.json");
        println!("  test <path>    runs the manifest at <path>");
        println!("  --help, -h     this message");
        return Ok(());
    }

    let print_egress_plan = raw_args.iter().any(|a| a == "--print-egress-plan");

    // `verify-patch` (F8): re-check that a patch still applies to its target.
    // Refuses on digest drift rather than silently verifying a different prompt.
    if let Some(pos) = raw_args.iter().position(|a| a == "verify-patch") {
        return verify_patch(&raw_args, pos);
    }

    let mut iter = raw_args.clone().into_iter();
    while let Some(arg) = iter.next() {
        if arg == "test" {
            if let Some(path) = iter.next() {
                manifest_path_str = path;
            }
        } else if !arg.starts_with("--") && arg != "verify-patch" {
            manifest_path_str = arg;
        }
    }

    // `verify-patch` (F8): refuse when the target's rules have drifted from
    // what the patch was derived from.
    if let Some(pos) = raw_args.iter().position(|a| a == "verify-patch") {
        return verify_patch(&raw_args, pos);
    }

    let manifest_path = Path::new(&manifest_path_str);
    if !manifest_path.exists() {
        anyhow::bail!("Path {:?} not found.", manifest_path);
    }

    // --print-egress-plan (F3): report the outbound data path for a manifest
    // without executing anything. Runs before any container starts.
    if print_egress_plan {
        let content = fs::read_to_string(manifest_path)
            .with_context(|| format!("Failed to read {:?}", manifest_path))?;
        let m: manifest::PlasticityManifest = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {:?}", manifest_path))?;
        let handling = m
            .optimization
            .data
            .as_ref()
            .map(|d| egress::DataHandling {
                data_class: d.data_class,
                egress: d.egress.clone(),
            })
            .unwrap_or_default();
        let has_llm_evaluators = m
            .evaluators
            .iter()
            .any(|e| e.r#type == manifest::EvaluatorType::Llm);
        print!(
            "{}",
            egress::render_egress_plan(
                &handling,
                &m.optimization.meta_llm.provider,
                has_llm_evaluators
            )
        );
        return Ok(());
    }

    let mut queue = Vec::new();
    if manifest_path.is_dir() {
        for entry in fs::read_dir(manifest_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().map_or(false, |e| e == "json") {
                queue.push(path);
            }
        }
        // Run via natural sort so 10-test.json comes AFTER 2-test.json (Issue #7)
        queue.sort_by(|a, b| {
            natord::compare(a.to_string_lossy().as_ref(), b.to_string_lossy().as_ref())
        });
    } else {
        queue.push(manifest_path.to_path_buf());
    }

    if queue.is_empty() {
        anyhow::bail!("No JSON manifests found in {:?}", manifest_path);
    }

    println!(
        "Detected {} test manifest(s). Commencing execution...",
        queue.len()
    );

    // Reap orphaned run containers from a previous orchestrator that was
    // SIGKILLed (kill_on_drop cannot fire then). First-run defect 2026-10-01:
    // a shell timeout killed the orchestrator and a neuro-run-* container kept
    // running its sandbox budget with nobody watching. Best-effort: any engine.
    for eng in ["podman", "docker"] {
        if crate::container::check_cmd(eng).await {
            if let Ok(out) = tokio::process::Command::new(eng)
                .args(["ps", "-aq", "--filter", "name=neuro-run-"])
                .output()
                .await
            {
                let ids = String::from_utf8_lossy(&out.stdout);
                for id in ids.lines().filter(|l| !l.trim().is_empty()) {
                    println!("♻️  Reaping orphaned container {}", id.trim());
                    let _ = tokio::process::Command::new(eng)
                        .args(["rm", "-f", id.trim()])
                        .output()
                        .await;
                }
            }
            break;
        }
    }

    // Filled in as manifests run; the patch is written from it at the end.
    let mut final_manifest: Option<manifest::PlasticityManifest> = None;
    let mut final_report: Option<patch::ManifestReport> = None;
    // The real outcome of the Waterfall, which is what the patch is allowed to
    // claim (F0). Derived, never assumed.
    let mut any_regression: Option<patch::RunOutcome> = None;
    let mut any_partial: Option<patch::RunOutcome> = None;
    // Set when a manifest's regression_guard policy is `block` and a regression
    // was seen: the patch is withheld entirely rather than annotated.
    let mut blocked_patch = false;
    // Protected rules the optimizer tried to change (F2).
    let mut quarantined_rules: Vec<rules::QuarantinedChange> = Vec::new();
    // Set when a manifest stopped on a budget limit (F7).
    let mut run_budget_halt: Option<String> = None;
    // The last transcript seen, for the patch artifact (F6a).
    let mut final_transcript: Option<transcript::Transcript> = None;
    // Which unit each rule addresses, for the patch (F5).
    let mut rule_units: Vec<Option<String>> = Vec::new();
    // Whether this patch still applies to its target (F8).
    let mut patch_provenance: Option<fingerprint::PatchProvenance> = None;
    // Grader agreement figures, when a quorum was used (F4b).
    let mut verdict_agreement: Vec<evaluator::Agreement> = Vec::new();
    // How each grader verdict was produced, for re-verifiability (F4a).
    let mut verdict_provenance: Vec<evaluator::VerdictProvenance> = Vec::new();

    let mut waterfall_restarts = 0;
    let max_restarts = queue.len() * 3; // Prevent infinite loops

    // The Adversarial Waterfall Loop
    'waterfall: loop {
        if waterfall_restarts > max_restarts {
            println!(
                "⚠️ Waterfall Loop Cap Reached ({} restarts). Aborting to prevent infinite loops.",
                max_restarts
            );
            any_partial.get_or_insert(patch::RunOutcome::Partial {
                manifest: "waterfall".to_string(),
                epochs: 0,
            });
            break 'waterfall;
        }
        let mut rules_mutated = false;

        for m in &queue {
            println!("\n=======================================================");
            println!("▶ Executing Manifest: {:?}", m);
            println!("=======================================================");

            let run = run_single_manifest(m).await?;
            let passed = run.passed;
            let epochs_taken = run.epochs_taken;
            let report = run.report;
            quarantined_rules.extend(run.quarantined);
            verdict_provenance.extend(run.provenance);
            if let Some(t) = run.transcript {
                final_transcript = Some(t);
            }
            patch_provenance = Some(run.patch_provenance);
            verdict_agreement.extend(run.agreement);
            rule_units.extend(run.rule_units);
            if let Some(reason) = run.budget_halt {
                run_budget_halt.get_or_insert(reason);
            }
            final_manifest = Some(run.manifest);

            // Evaluate every regression-related fact before `report` is moved
            // into `final_report`.
            let manifest_name = report.manifest_name.clone();
            let regressions: Vec<patch::EvaluatorOutcome> =
                report.regressions().into_iter().cloned().collect();
            let improvements: Vec<String> = report
                .improvements()
                .into_iter()
                .map(|r| r.name.clone())
                .collect();

            if !regressions.is_empty() {
                let names: Vec<String> = regressions.iter().map(|r| r.name.clone()).collect();
                println!(
                    "\n🚨 REGRESSION: {} evaluator(s) that passed at baseline now fail: {}",
                    names.len(),
                    names.join(", ")
                );
                for r in &regressions {
                    println!("     - {}", r.name);
                }
                println!(
                    "   A new rule fixed something by breaking this. The patch will be marked REGRESSED."
                );
                any_regression.get_or_insert(patch::RunOutcome::Regressed {
                    manifest: manifest_name.clone(),
                    evaluators: names,
                });
                if run.blocked_by_regression {
                    blocked_patch = true;
                    println!(
                        "   🛑 regression_guard policy is `block`: the patch will be withheld."
                    );
                }
            }
            if !improvements.is_empty() {
                println!("   ✨ Improved this epoch: {}", improvements.join(", "));
            }
            final_report = Some(report);

            if !passed {
                println!(
                    "\n⚠️  Manifest {:?} failed to pass even after {} epochs.",
                    m, epochs_taken
                );
                println!(
                    "   Halting the Waterfall here, but preserving the rules accumulated so far."
                );
                println!("   These rules are UNVERIFIED — a proposal for review, not a fix.");
                any_partial.get_or_insert(patch::RunOutcome::Partial {
                    manifest: manifest_name.clone(),
                    epochs: epochs_taken,
                });
                break 'waterfall;
            }

            if epochs_taken > 1 {
                rules_mutated = true;
                waterfall_restarts += 1;
                println!(
                    "\n🔄 Rules were mutated by {:?}. Restarting Waterfall from the top to ensure backward compatibility...",
                    m
                );
                break; // Break the inner loop to restart the waterfall
            }
        }

        if !rules_mutated {
            println!(
                "\n🌊 Waterfall Complete! All models passed on Epoch 1. No further rule mutations were needed."
            );
            break 'waterfall;
        }
    }

    // A regression is the most severe outcome: it outranks everything else. A
    // budget halt outranks a partial run, because the halt is why the run never
    // reached a conclusion — the partial failure is a consequence, not the
    // finding (F7).
    let outcome = match (any_regression, any_partial, run_budget_halt.take()) {
        (Some(regression), _, _) => regression,
        (None, _, Some(reason)) => patch::RunOutcome::BudgetHalted { reason },
        (None, Some(partial), None) => partial,
        (None, None, None) => patch::RunOutcome::Verified,
    };

    // Write final patch
    if blocked_patch {
        // `block` policy: emit nothing that could be mistaken for a fix. The
        // rules stay on disk for inspection, but no patch artifact is produced.
        if Path::new("neuroplasticity_patch.md").exists() {
            let _ = std::fs::remove_file("neuroplasticity_patch.md");
            println!(
                "\n🛑 Removed neuroplasticity_patch.md: regression_guard policy is `block` and a \
                 previously-passing evaluator regressed."
            );
        }
    } else if let Some(manifest) = final_manifest {
        let target_rules_file = Path::new(&manifest.optimization.target_rules_file);
        if target_rules_file.exists() {
            if let Ok(content) = fs::read_to_string(target_rules_file) {
                // Accept both bare strings and classified objects (F2).
                if let Ok(parsed) = rules::parse_rules(&content) {
                    let rule_texts = rules::rule_texts(&parsed);

                    if !rule_texts.is_empty() {
                        let patch_doc = patch::render_patch(
                            &manifest.name,
                            &outcome,
                            final_report.as_ref(),
                            &rule_texts,
                            &quarantined_rules,
                            &verdict_provenance,
                            final_transcript.as_ref(),
                            &rule_units,
                            patch_provenance.as_ref(),
                            &verdict_agreement,
                        );

                        let patch_path = Path::new("neuroplasticity_patch.md");
                        if fs::write(patch_path, patch_doc).is_ok() {
                            println!("\n📄 Improvement patch generated at {:?}", patch_path);
                            if !outcome.is_verified() {
                                println!(
                                    "   ⚠️  Status: {:?}. The rules are NOT verified — review before adopting.",
                                    outcome
                                );
                            }
                            println!(
                                "   Provide this patch file to your primary agent to {}.",
                                if outcome.is_verified() {
                                    "permanently implement the fix."
                                } else {
                                    "review the unverified rules."
                                }
                            );
                        }
                    }
                }
            }
        }
    }

    // A run that ends with a regression is not a success (F1), so it must not
    // report itself as one.
    if !outcome.is_verified() {
        println!("\n🛑 Run did not converge cleanly: {:?}", outcome);
        std::process::exit(2);
    }

    Ok(())
}

#[cfg(test)]
mod provenance_header_tests {
    use super::extract_digest;

    const PATCH: &str = concat!(
        "# Patch\n\n",
        "**Provenance**\n\n",
        "- **Manifest hash:** `sha256:aaaa`\n",
        "- **Evaluator set hash:** `sha256:bbbb`\n",
        "- **Baseline target rules digest:** `sha256:cccc`\n",
        "- **Run at:** `2026-09-28T02:40:57Z`\n",
    );

    #[test]
    fn extracts_a_field_from_the_provenance_header() {
        assert_eq!(
            extract_digest(PATCH, "Baseline target rules digest").as_deref(),
            Some("sha256:cccc")
        );
        assert_eq!(
            extract_digest(PATCH, "Manifest hash").as_deref(),
            Some("sha256:aaaa")
        );
    }

    /// A value containing colons must survive intact.
    #[test]
    fn digest_colons_are_not_split() {
        let got = extract_digest(PATCH, "Run at").unwrap();
        assert_eq!(got, "2026-09-28T02:40:57Z");
    }

    #[test]
    fn absent_field_returns_none() {
        assert!(extract_digest(PATCH, "Transcript digest").is_none());
        assert!(extract_digest("no provenance here", "Manifest hash").is_none());
    }
}
