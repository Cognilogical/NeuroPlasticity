use crate::manifest::{Evaluator, EvaluatorType, Sandbox, MetaLlmConfig};
use crate::llm_client::ask_llm;
use anyhow::{Context, Result};
use std::process::Command;
use std::path::Path;

#[derive(Debug)]
pub struct EvaluatorScore {
    pub name: String,
    pub success: bool,
    pub weight: f64,
    pub output: Option<String>, // Useful to capture why an LLM or container failed
}

#[derive(Debug)]
pub struct EvaluationResult {
    pub pass: bool,
    pub score: f64,
    pub total_weight: f64,
    pub passing_weight: f64,
    pub threshold: f64,
    pub details: Vec<EvaluatorScore>,
}

pub async fn evaluate(
    evaluators: &[Evaluator],
    working_dir: &Path,
    pass_threshold: f64,
    sandbox: &Sandbox,
    meta_llm: &MetaLlmConfig,
) -> Result<EvaluationResult> {
    let mut total_weight = 0.0;
    let mut passing_weight = 0.0;
    let mut details = Vec::new();

    for eval in evaluators {
        total_weight += eval.weight;

        let (success, output) = match eval.r#type {
            EvaluatorType::HostBash => {
                if let Some(script) = &eval.script {
                    if script.is_empty() {
                        (false, Some("Empty host_bash script array".to_string()))
                    } else {
                        let mut cmd = Command::new(&script[0]);
                        if script.len() > 1 {
                            cmd.args(&script[1..]);
                        }
                        cmd.current_dir(working_dir);
                        let output = cmd.output().ok();
                        if let Some(out) = output {
                            let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                            if msg.is_empty() {
                                msg = String::from_utf8_lossy(&out.stdout).to_string();
                            }
                            (out.status.success(), Some(msg))
                        } else {
                            (false, Some("Failed to execute command".to_string()))
                        }
                    }
                } else {
                    (false, Some("Missing 'script' for host_bash evaluator".to_string()))
                }
            },
            EvaluatorType::Container => {
                if let (Some(image), Some(command)) = (&eval.image, &eval.command) {
                    // Re-use the agent runner, but without mounting the project, or just mount workspace as RO
                    // For simplicity, we use the existing runner::run_agent logic, but tuned for evaluation.
                    let (engine, is_podman) = detect_container_engine(&sandbox.engine).unwrap_or(("podman".to_string(), true));
                    let mut cmd = Command::new(&engine);
                    cmd.arg("run");
                    cmd.arg("--rm");
                    if is_podman {
                        cmd.arg("--userns=keep-id");
                    }
                    cmd.arg("--security-opt");
                    cmd.arg("no-new-privileges");
                    
                    // Mount the scratch workspace as read-only for evaluation
                    let scratch_mount = sandbox.workspace.as_ref().map_or("/workspace", |w| &w.scratch_mount);
                    cmd.arg("-v");
                    cmd.arg(&format!("{}:{}:ro,Z", working_dir.display(), scratch_mount));
                    cmd.arg("--workdir");
                    cmd.arg(scratch_mount);
                    
                    cmd.arg(image);
                    
                    if let Some(setup) = &eval.setup_script {
                        if !setup.is_empty() {
                            let joined_script = setup.join(" && ");
                            let quoted_cmd: Vec<String> = command.iter().map(|s| {
                                if s.contains(' ') || s.contains('"') || s.contains('\'') {
                                    format!("'{}'", s.replace('\'', "'\\''"))
                                } else { s.clone() }
                            }).collect();
                            let full_command = format!("{} && {}", joined_script, quoted_cmd.join(" "));
                            cmd.arg("sh");
                            cmd.arg("-c");
                            cmd.arg(&full_command);
                        } else {
                            cmd.args(command);
                        }
                    } else {
                        cmd.args(command);
                    }

                    if let Ok(out) = cmd.output() {
                        let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                        if msg.is_empty() {
                            msg = String::from_utf8_lossy(&out.stdout).to_string();
                        }
                        (out.status.success(), Some(msg))
                    } else {
                        (false, Some("Failed to run container evaluator".to_string()))
                    }
                } else {
                    (false, Some("Missing 'image' or 'command' for container evaluator".to_string()))
                }
            },
            EvaluatorType::Llm => {
                if let (Some(prompt), Some(target_file)) = (&eval.prompt, &eval.target_file) {
                    let file_path = working_dir.join(target_file);
                    if file_path.exists() {
                        if let Ok(content) = std::fs::read_to_string(&file_path) {
                            let system_prompt = "You are an automated evaluator. Grade the user's document based on the provided prompt. Reply with exactly 'PASS' or 'FAIL' followed by a brief reason.";
                            let user_prompt = format!("Evaluation Prompt:\n{}\n\nTarget Document ({}):\n{}", prompt, target_file, content);
                            
                            match ask_llm(meta_llm, system_prompt, &user_prompt).await {
                                Ok(response) => {
                                    println!("LLM Evaluator '{}' Response: {}", eval.name, response);
                                    let success = response.trim().starts_with("PASS");
                                    (success, Some(response))
                                },
                                Err(e) => (false, Some(format!("LLM Error: {}", e)))
                            }
                        } else {
                            (false, Some(format!("Failed to read target file: {}", target_file)))
                        }
                    } else {
                        (false, Some(format!("Target file does not exist: {}", target_file)))
                    }
                } else {
                    (false, Some("Missing 'prompt' or 'target_file' for llm evaluator".to_string()))
                }
            }
        };

        if success {
            passing_weight += eval.weight;
        }

        details.push(EvaluatorScore {
            name: eval.name.clone(),
            success,
            weight: eval.weight,
            output,
        });
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
    })
}

// Duplicated from runner for simplicity, alternatively make it pub in runner.rs
fn detect_container_engine(preferred: &str) -> Result<(String, bool)> {
    let check_cmd = |cmd: &str| -> bool {
        Command::new(cmd).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
    };
    if preferred == "docker" && check_cmd("docker") { return Ok(("docker".to_string(), false)); }
    if preferred == "podman" && check_cmd("podman") { return Ok(("podman".to_string(), true)); }
    if check_cmd("podman") { return Ok(("podman".to_string(), true)); }
    if check_cmd("docker") { return Ok(("docker".to_string(), false)); }
    anyhow::bail!("No container engine found.");
}
