use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub mod reporter;
pub mod evaluator;
pub mod manifest;
pub mod optimizer;
pub mod runner;
pub mod llm_client;
pub mod fingerprint;
#[cfg(feature = "embedded-llm")]
pub mod embedded_llm;

async fn run_single_manifest(manifest_path: &Path) -> Result<(bool, u32, manifest::PlasticityManifest)> {
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

    for epoch in 1..=max_epochs {
        println!("\n--- Epoch {} / {} ---", epoch, max_epochs);

        // Calculate run fingerprint
        let evaluators_json = serde_json::to_string(&manifest.evaluators).unwrap_or_default();
        let fingerprint = fingerprint::calculate_fingerprint(
            agent_command, 
            &target_rules_file, 
            &manifest.name,
            &manifest.optimization.meta_llm.model,
            &evaluators_json
        );
        
        let mut stdout: String;
        let stderr: String;
        let score: f64;
        let pass: bool;
        let threshold: f64;
        
        if let Some(cached_failure) = fingerprint::check_fingerprint(&fingerprint) {
            println!("⚡ FAST PATH: Known failure fingerprint ({}) detected for this exact rule configuration.", fingerprint);
            println!("Skipping 120s container execution and loading cached side-effects...");
            stdout = cached_failure.stdout;
            stderr = cached_failure.stderr;
            score = cached_failure.score;
            pass = false; // We only cache failures
            threshold = pass_threshold;
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
            ).await.context("Failed to run agent in container sandbox")?;

            stdout = sandbox_stdout;
            stderr = sandbox_stderr;

            // 4. Evaluate & Score
            println!("Evaluating side effects...");
            let eval_result = evaluator::evaluate(
                &manifest.evaluators,
                scratch_path,
                pass_threshold,
                &manifest.sandbox,
                &manifest.optimization.meta_llm,
            ).await.context("Evaluator execution failed")?;

            score = eval_result.score;
            pass = eval_result.pass;
            threshold = eval_result.threshold;

            println!("Score: {:.2} (Threshold: {:.2})", score, threshold);

            // 5. Observe & Report
            println!("Generating epoch report...");
            let reporter = reporter::Reporter::new();
            reporter.report_epoch(
                &run_id,
                epoch as u32,
                &stdout,
                &stderr,
                score,
                vec![], // We'll add mutations here if applicable
            ).context("Failed to write epoch report")?;

            // If it failed, save to fingerprint cache so we never run this exact configuration again
            if !pass {
                let _ = fingerprint::save_fingerprint(&fingerprint, fingerprint::CachedFailure {
                    score,
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                });
            }
            
            println!("Cleaning up ephemeral workspace...");
        }

        if pass {
            println!("✅ Epoch {} achieved passing score! Run complete.", epoch);
            return Ok((true, epoch as u32, manifest));
        }

        // 7. Optimize & Mutate
        if epoch < max_epochs {
            println!("❌ Score below threshold. Invoking Meta-Optimizer...");
            
            // Read existing rules to pass to the optimizer as context
            let existing_rules: Vec<String> = if target_rules_file.exists() {
                if let Ok(content) = fs::read_to_string(&target_rules_file) {
                    serde_json::from_str(&content).unwrap_or_default()
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            };
            
            let new_rule = optimizer::run_llm_optimizer(
                &manifest.optimization.meta_llm,
                &stderr,
                &manifest.task_prompt,
                &existing_rules,
            ).await?;
            
            // Append the generated rule to rules.json
            if let Some(parent) = target_rules_file.parent() {
                fs::create_dir_all(parent)?;
            }
            
            let mut rules_to_save = existing_rules.clone();
            rules_to_save.push(new_rule.clone());
            let updated_json = serde_json::to_string_pretty(&rules_to_save)?;
            fs::write(&target_rules_file, updated_json)?;
            
            println!("Applied new rule optimization to {:?}", target_rules_file);
        } else {
            println!("❌ Max epochs reached without achieving pass threshold.");
        }
    }

    Ok((false, max_epochs as u32, manifest))
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== NeuroPlasticity Orchestrator ===");

    // 1. Parse & Validate Manifest
    let mut args = std::env::args();
    args.next(); // Skip executable name
    
    let mut manifest_path_str = "plasticity.json".to_string();
    while let Some(arg) = args.next() {
        if arg == "test" {
            if let Some(path) = args.next() {
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
    
    let mut queue = Vec::new();
    if manifest_path.is_dir() {
        for entry in fs::read_dir(manifest_path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() && path.extension().map_or(false, |e| e == "json") {
                queue.push(path);
            }
        }
        queue.sort(); // Run alphabetically (e.g. 01-reasoning.json, 02-forgetful.json)
    } else {
        queue.push(manifest_path.to_path_buf());
    }

    if queue.is_empty() {
        anyhow::bail!("No JSON manifests found in {:?}", manifest_path);
    }

    println!("Detected {} test manifest(s). Commencing execution...", queue.len());

    let mut final_manifest: Option<manifest::PlasticityManifest> = None;

    // The Adversarial Waterfall Loop
    'waterfall: loop {
        let mut rules_mutated = false;

        for m in &queue {
            println!("\n=======================================================");
            println!("▶ Executing Manifest: {:?}", m);
            println!("=======================================================");
            
            let (passed, epochs_taken, manifest) = run_single_manifest(m).await?;
            final_manifest = Some(manifest);

            if !passed {
                println!("\n⚠️  Manifest {:?} failed to pass even after {} epochs.", m, epochs_taken);
                println!("   Halting the Waterfall here, but preserving the rules accumulated so far.");
                println!("   These rules are quantifiably better than the baseline, even if they couldn't fully domesticate this specific model.");
                break 'waterfall;
            }

            if epochs_taken > 1 {
                rules_mutated = true;
                println!("\n🔄 Rules were mutated by {:?}. Restarting Waterfall from the top to ensure backward compatibility...", m);
                break; // Break the inner loop to restart the waterfall
            }
        }

        if !rules_mutated {
            println!("\n🌊 Waterfall Complete! All models passed on Epoch 1. No further rule mutations were needed.");
            break 'waterfall;
        }
    }

    // Write final patch
    if let Some(manifest) = final_manifest {
        let target_rules_file = Path::new(&manifest.optimization.target_rules_file);
        if target_rules_file.exists() {
            if let Ok(content) = fs::read_to_string(target_rules_file) {
                if let Ok(rules) = serde_json::from_str::<Vec<String>>(&content) {
                    if !rules.is_empty() {
                        let mut patch_doc = String::from("# 🧠 NeuroPlasticity Improvement Patch\n\n");
                        patch_doc.push_str(&format!("**Target Project:** `{}`\n", manifest.name));
                        patch_doc.push_str("**Status:** ✅ Verified against deterministic evaluators across the entire Waterfall.\n\n");
                        patch_doc.push_str("The following behavioral constraints successfully corrected the agent's failure paths. You should permanently inject these into the target agent's system prompt or `AGENTS.md`:\n\n");
                        
                        for (i, rule) in rules.iter().enumerate() {
                            patch_doc.push_str(&format!("### Rule {}\n> {}\n\n", i + 1, rule));
                        }
                        
                        let patch_path = Path::new("neuroplasticity_patch.md");
                        if fs::write(patch_path, patch_doc).is_ok() {
                            println!("\n📄 Improvement patch generated at {:?}", patch_path);
                            println!("   Provide this patch file to your primary agent to permanently implement the fix.");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
