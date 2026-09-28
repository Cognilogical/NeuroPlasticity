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

    // Baseline is captured on the first epoch that actually evaluates, before
    // any rule has been mutated, and never re-captured.
    let mut baseline: Option<Vec<patch::EvaluatorOutcome>> = None;
    let mut final_results: Vec<patch::EvaluatorOutcome> = Vec::new();
    // Protected-rule changes reverted along the way (F2).
    let mut quarantined: Vec<rules::QuarantinedChange> = Vec::new();

    for epoch in 1..=max_epochs {
        println!("\n--- Epoch {} / {} ---", epoch, max_epochs);

        // Calculate run fingerprint
        let evaluators_json = serde_json::to_string(&manifest.evaluators).unwrap_or_default();
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

            // 4. Evaluate & Score
            println!("Evaluating side effects...");
            let eval_result = evaluator::evaluate(
                &manifest.evaluators,
                scratch_path,
                pass_threshold,
                &manifest.sandbox,
                &manifest.optimization.meta_llm,
                &data_handling,
            )
            .await
            .context("Evaluator execution failed")?;

            score = eval_result.score;
            pass = eval_result.pass;

            // Record the baseline on the first evaluated epoch, and track the
            // final result set for the delta diff (F1).
            let outcomes = patch::EvaluatorOutcome::from_result(&eval_result);
            if baseline.is_none() {
                baseline = Some(outcomes.clone());
            }
            final_results = outcomes;

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
            return Ok(ManifestRun {
                passed: true,
                epochs_taken: epoch as u32,
                blocked_by_regression: false,
                quarantined,
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

            let new_rule = optimizer::run_llm_optimizer(
                &manifest.optimization.meta_llm,
                &stderr,
                &manifest.task_prompt,
                &optimizer_visible,
            )
            .await
            .with_context(|| {
                "The Meta-Optimizer could not produce a usable new rule. The agent is not \
                 necessarily still at fault — verify the meta_llm endpoint and key before \
                 trusting any further optimization."
            })?;

            // Append the generated rule to rules.json
            if let Some(parent) = target_rules_file.parent() {
                fs::create_dir_all(parent)?;
            }

            let mut proposed = existing_rules.clone();
            proposed.push(Rule::behavioral(new_rule.clone()));

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
        } else {
            println!("❌ Max epochs reached without achieving pass threshold.");
        }
    }

    let baseline_outcomes = baseline.unwrap_or_default();
    // `block` refuses to present a rule set that regressed something green.
    let blocked_by_regression = manifest
        .optimization
        .regression_guard
        .as_ref()
        .map(|g| g.policy == manifest::RegressionPolicy::Block)
        .unwrap_or(false)
        && patch::ManifestReport {
            manifest_name: manifest.name.clone(),
            passed: false,
            epochs_taken: max_epochs as u32,
            baseline: baseline_outcomes.clone(),
            final_results: final_results.clone(),
        }
        .regressions()
        .len()
            > 0;

    Ok(ManifestRun {
        passed: false,
        epochs_taken: max_epochs as u32,
        blocked_by_regression,
        quarantined,
        report: patch::ManifestReport {
            manifest_name: manifest.name.clone(),
            passed: false,
            epochs_taken: max_epochs as u32,
            baseline: baseline_outcomes,
            final_results,
        },
        manifest,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== NeuroPlasticity Orchestrator ===");

    // 1. Parse & Validate Manifest
    let mut args = std::env::args();
    args.next(); // Skip executable name

    let mut manifest_path_str = "plasticity.json".to_string();
    // Collected because std::env::Args is not Clone and the flag is read twice.
    let raw_args: Vec<String> = args.by_ref().collect();
    let print_egress_plan = raw_args.iter().any(|a| a == "--print-egress-plan");

    let mut iter = raw_args.into_iter();
    while let Some(arg) = iter.next() {
        if arg == "test" {
            if let Some(path) = iter.next() {
                manifest_path_str = path;
            }
        } else if !arg.starts_with("--") {
            manifest_path_str = arg;
        }
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

    // A regression is the most severe outcome: it outranks a partial run.
    let outcome = match (any_regression, any_partial) {
        (Some(regression), _) => regression,
        (None, Some(partial)) => partial,
        (None, None) => patch::RunOutcome::Verified,
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
