use crate::manifest::{Evaluator, EvaluatorType, Sandbox, MetaLlmConfig};
use crate::llm_client::ask_llm;
use anyhow::Result;
use std::path::Path;
use tokio::process::Command;
use futures::future::join_all;
use std::sync::Arc;
use tokio::sync::Semaphore;

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
    let mut futures = Vec::new();

    // Prevent system resources from being crushed by the embedded LLM.
    // If the provider is 'embedded', strictly limit LLM evaluation concurrency to 1.
    // If it's a cloud provider (GitHub, OpenAI, Anthropic), allow up to 10 concurrent requests.
    let is_embedded = meta_llm.provider == "embedded";
    let llm_concurrency = if is_embedded { 1 } else { 10 };
    let llm_semaphore = Arc::new(Semaphore::new(llm_concurrency));

    // Spawn each evaluator into an asynchronous task so they execute in parallel
    for eval in evaluators {
        let eval_clone = eval.clone();
        let working_dir_clone = working_dir.to_path_buf();
        let sandbox_clone = sandbox.clone();
        let meta_llm_clone = meta_llm.clone();
        let llm_sem_clone = Arc::clone(&llm_semaphore);

        let handle = tokio::spawn(async move {
            let (success, output) = match eval_clone.r#type {
                EvaluatorType::HostBash => {
                    if let Some(script) = &eval_clone.script {
                        if script.is_empty() {
                            (false, Some("Empty host_bash script array".to_string()))
                        } else {
                            let mut cmd = Command::new(&script[0]);
                            if script.len() > 1 {
                                cmd.args(&script[1..]);
                            }
                            cmd.current_dir(&working_dir_clone);
                            
                            if let Ok(out) = cmd.output().await {
                                let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                                if msg.is_empty() {
                                    msg = String::from_utf8_lossy(&out.stdout).to_string();
                                }
                                println!("Evaluator '{}' output: {}", eval_clone.name, msg);
                                (out.status.success(), Some(msg))
                            } else {
                                (false, Some("Failed to execute host_bash command".to_string()))
                            }
                        }
                    } else {
                        (false, Some("Missing 'script' for host_bash evaluator".to_string()))
                    }
                },
                EvaluatorType::Container => {
                    if let (Some(image), Some(command)) = (&eval_clone.image, &eval_clone.command) {
                        let preferred_engine = Some(sandbox_clone.engine.clone());
                        let (engine, is_podman) = crate::container::detect_container_engine(&preferred_engine)
                            .await
                            .unwrap_or(("podman".to_string(), true));
                            
                        let mut cmd = Command::new(&engine);
                        cmd.arg("run");
                        cmd.arg("--rm");
                        if is_podman {
                            cmd.arg("--userns=keep-id");
                        }
                        cmd.arg("--security-opt");
                        cmd.arg("no-new-privileges");
                        
                        let scratch_mount = sandbox_clone.workspace.as_ref().map_or("/workspace", |w| &w.scratch_mount);
                        cmd.arg("-v");
                        cmd.arg(&format!("{}:{}:ro,Z", working_dir_clone.display(), scratch_mount));
                        cmd.arg("--workdir");
                        cmd.arg(scratch_mount);
                        
                        cmd.arg(image);
                        
                        if let Some(setup) = &eval_clone.setup_script {
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

                        if let Ok(out) = cmd.output().await {
                            let mut msg = String::from_utf8_lossy(&out.stderr).to_string();
                            if msg.is_empty() {
                                msg = String::from_utf8_lossy(&out.stdout).to_string();
                            }
                            println!("Evaluator '{}' output: {}", eval_clone.name, msg);
                            (out.status.success(), Some(msg))
                        } else {
                            (false, Some("Failed to run container evaluator".to_string()))
                        }
                    } else {
                        (false, Some("Missing 'image' or 'command' for container evaluator".to_string()))
                    }
                },
                EvaluatorType::Llm => {
                    let _permit = llm_sem_clone.acquire().await.expect("Failed to acquire LLM semaphore");
                    
                    if let (Some(prompt), Some(target_file)) = (&eval_clone.prompt, &eval_clone.target_file) {
                        let file_path = working_dir_clone.join(target_file);
                        if file_path.exists() {
                            if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                                let system_prompt = "You are an automated evaluator. Grade the user's document based on the provided prompt. Reply with exactly 'PASS' or 'FAIL' followed by a brief reason.";
                                let user_prompt = format!("Evaluation Prompt:\n{}\n\nTarget Document ({}):\n{}", prompt, target_file, content);
                                
                                match ask_llm(&meta_llm_clone, system_prompt, &user_prompt).await {
                                    Ok(response) => {
                                        println!("LLM Evaluator '{}' Response: {}", eval_clone.name, response);
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
            
            EvaluatorScore {
                name: eval_clone.name,
                success,
                weight: eval_clone.weight,
                output,
            }
        });

        futures.push(handle);
    }

    // Await all evaluators in parallel
    let results = join_all(futures).await;

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
    })
}
