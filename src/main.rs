use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use uuid::Uuid;

pub mod reporter;
pub mod evaluator;
pub mod manifest;
pub mod optimizer;
pub mod runner;
pub mod llm_client;
#[cfg(feature = "embedded-llm")]
pub mod embedded_llm;

#[tokio::main]
async fn main() -> Result<()> {
    println!("=== NeuroPlasticity Orchestrator ===");

    // 1. Parse & Validate Manifest
    let mut args = std::env::args();
    // Skip executable name
    args.next();
    
    // Parse arguments
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
        anyhow::bail!("Manifest file {:?} not found.", manifest_path);
    }
    
    let manifest_content = fs::read_to_string(manifest_path)
        .context("Failed to read plasticity.json")?;
    let manifest: manifest::PlasticityManifest = serde_json::from_str(&manifest_content)
        .context("Failed to parse plasticity.json")?;

    let run_id = Uuid::new_v4().to_string();
    println!("Starting Run ID: {}", run_id);
    println!("Target Project: {}", manifest.name);

    let max_epochs = manifest.optimization.epochs;
    let pass_threshold = manifest.optimization.pass_threshold;
    let base_image = &manifest.sandbox.base_image;
    let agent_command = &manifest.agent_command;

    for epoch in 1..=max_epochs {
        println!("\n--- Epoch {} / {} ---", epoch, max_epochs);

        // 2. Isolate: Setup scratch workspace
        println!("Setting up ephemeral workspace...");
        let scratch_dir = runner::setup_workspace(Path::new("."))
            .context("Failed to setup ephemeral workspace")?;
        let scratch_path = scratch_dir.path();

        // 3. Execute Agent in Podman
        println!("Executing agent in sandbox ({})...", base_image);
        let (stdout, stderr, _success) = runner::run_agent(
            Path::new("."),
            scratch_path,
            &manifest.sandbox,
            agent_command,
        ).context("Failed to run agent in container sandbox")?;

        // 4. Evaluate & Score
        println!("Evaluating side effects...");
        let eval_result = evaluator::evaluate(
            &manifest.evaluators,
            scratch_path,
            pass_threshold,
            &manifest.sandbox,
            &manifest.optimization.meta_llm,
        ).await.context("Evaluator execution failed")?;

        println!("Score: {:.2} (Threshold: {:.2})", eval_result.score, eval_result.threshold);

        // 5. Observe & Report
        println!("Generating epoch report...");
        let reporter = reporter::Reporter::new();
        reporter.report_epoch(
            &run_id,
            epoch as u32,
            &stdout,
            &stderr,
            eval_result.score,
            vec![], // We'll add mutations here if applicable
        ).context("Failed to write epoch report")?;

        // 6. Check for success
        if eval_result.pass {
            println!("✅ Epoch {} achieved passing score! Run complete.", epoch);

            if epoch > 1 {
                // If it took > 1 epoch, that means the mutated rules successfully fixed the agent.
                // We should output an implementation patch.
                let target_rules_file = Path::new(&manifest.optimization.target_rules_file);
                if target_rules_file.exists() {
                    if let Ok(content) = fs::read_to_string(target_rules_file) {
                        if let Ok(rules) = serde_json::from_str::<Vec<String>>(&content) {
                            if !rules.is_empty() {
                                let mut patch_doc = String::from("# 🧠 NeuroPlasticity Improvement Patch\n\n");
                                patch_doc.push_str(&format!("**Target Project:** `{}`\n", manifest.name));
                                patch_doc.push_str("**Status:** ✅ Verified against determinisitic evaluators.\n\n");
                                patch_doc.push_str("The following behavioral constraints successfully corrected the agent's failure paths. You should permanently inject these into the target agent's system prompt or `AGENTS.md`:\n\n");
                                
                                for (i, rule) in rules.iter().enumerate() {
                                    patch_doc.push_str(&format!("### Rule {}\n> {}\n\n", i + 1, rule));
                                }
                                
                                let patch_path = Path::new("neuroplasticity_patch.md");
                                if fs::write(patch_path, patch_doc).is_ok() {
                                    println!("📄 Improvement patch generated at {:?}", patch_path);
                                    println!("   Provide this patch file to your primary agent to permanently implement the fix.");
                                }
                            }
                        }
                    }
                }
            } else {
                println!("✅ Passed on first try. No rule mutations were required.");
            }

            break;
        }

        // 7. Optimize & Mutate
        if epoch < max_epochs {
            println!("❌ Score below threshold. Invoking Meta-Optimizer...");
            let new_rule = optimizer::run_llm_optimizer(
                &manifest.optimization.meta_llm,
                &stderr,
                &manifest.task_prompt,
            ).await?;
            
            // Append the generated rule to rules.json
            let target_rules_file = Path::new(&manifest.optimization.target_rules_file);
            if let Some(parent) = target_rules_file.parent() {
                fs::create_dir_all(parent)?;
            }
            
            let mut existing_rules: Vec<String> = if target_rules_file.exists() {
                let content = fs::read_to_string(target_rules_file)?;
                serde_json::from_str(&content).unwrap_or_default()
            } else {
                Vec::new()
            };
            
            existing_rules.push(new_rule.clone());
            let updated_json = serde_json::to_string_pretty(&existing_rules)?;
            fs::write(target_rules_file, updated_json)?;
            
            println!("Applied new rule optimization to {:?}", target_rules_file);
        } else {
            println!("❌ Max epochs reached without achieving pass threshold.");
        }
        
        // Scratch dir is automatically cleaned up here as `scratch_dir` goes out of scope
        println!("Cleaning up ephemeral workspace...");
    }

    Ok(())
}
