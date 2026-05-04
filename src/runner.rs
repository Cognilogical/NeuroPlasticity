use std::path::{Path, PathBuf};
use anyhow::Result;
use tempfile::TempDir;
use crate::manifest::Sandbox;
use tokio::process::Command;
use uuid::Uuid;

pub struct RunnerResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub scratch_dir: TempDir,
}

pub fn setup_workspace(_project_dir: &Path) -> Result<TempDir> {
    let scratch_dir = tempfile::Builder::new().prefix("neuroplasticity-run-").tempdir()?;
    Ok(scratch_dir)
}

pub async fn run_agent(
    project_dir: &Path,
    scratch_path: &Path,
    sandbox: &Sandbox,
    agent_command: &[String],
) -> Result<(String, String, bool)> {
    let preferred_engine = Some(sandbox.engine.clone());
    let (engine, is_podman) = crate::container::detect_container_engine(&preferred_engine).await?;
    let container_name = format!("neuro-run-{}", Uuid::new_v4().to_string().replace("-", ""));
    
    let mut cmd = Command::new(&engine);
    cmd.arg("run");
    cmd.arg("--rm");
    cmd.arg("--name");
    cmd.arg(&container_name);
    
    if is_podman {
        cmd.arg("--userns=keep-id");
    } else {
        if let Ok(uid_out) = tokio::process::Command::new("id").arg("-u").output().await {
            if let Ok(gid_out) = tokio::process::Command::new("id").arg("-g").output().await {
                let uid = String::from_utf8_lossy(&uid_out.stdout).trim().to_string();
                let gid = String::from_utf8_lossy(&gid_out.stdout).trim().to_string();
                if !uid.is_empty() && !gid.is_empty() {
                    cmd.arg(format!("--user={}:{}", uid, gid));
                }
            }
        }
    }
    
    cmd.arg("--security-opt");
    cmd.arg("no-new-privileges");
    
    let project_mount = sandbox.workspace.as_ref().map_or("/project", |w| &w.project_mount);
    let scratch_mount = sandbox.workspace.as_ref().map_or("/workspace", |w| &w.scratch_mount);

    cmd.arg("-v");
    cmd.arg(&format!("{}:{}:ro,Z", project_dir.display(), project_mount));

    cmd.arg("-v");
    cmd.arg(&format!("{}:{}:rw,Z", scratch_path.display(), scratch_mount));

    let ephemeral_home = tempfile::Builder::new().prefix("neuro-home-").tempdir()?;
    cmd.arg("-v");
    cmd.arg(&format!("{}:/user_home:rw,Z", ephemeral_home.path().display()));
    
    cmd.arg("-e").arg("HOME=/user_home");
    cmd.arg("-e").arg("NPM_CONFIG_PREFIX=/user_home/.npm-global");
    cmd.arg("-e").arg("PATH=/user_home/.local/bin:/user_home/.npm-global/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");

    cmd.arg("-e").arg("CI=true");
    cmd.arg("-e").arg("CONTINUOUS_INTEGRATION=1");
    cmd.arg("-e").arg("NONINTERACTIVE=1");
    cmd.arg("-e").arg("DEBIAN_FRONTEND=noninteractive");

    if let Some(mounts) = &sandbox.mounts {
        for mount in mounts {
            let expanded_source = shellexpand::tilde(&mount.source).to_string();
            let source_path = PathBuf::from(expanded_source);
            if !source_path.exists() {
                println!("⚠️  Mount source {} does not exist, skipping.", mount.source);
                continue;
            }
            
            if mount.target.starts_with("/user_home/") {
                if let Ok(rel_path) = Path::new(&mount.target).strip_prefix("/user_home/") {
                    let host_target_dir = ephemeral_home.path().join(rel_path);
                    if source_path.is_dir() {
                        let _ = std::fs::create_dir_all(&host_target_dir);
                    } else if let Some(parent) = host_target_dir.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                }
            }

            let ro_flag = if mount.readonly { "ro," } else { "" };
            cmd.arg("-v");
            cmd.arg(&format!("{}:{}:{}Z", source_path.display(), mount.target, ro_flag));
        }
    }

    cmd.arg("--workdir");
    cmd.arg(scratch_mount);
    cmd.arg(&sandbox.base_image);

    if let Some(setup_script) = &sandbox.setup_script {
        if !setup_script.is_empty() {
            let joined_script = setup_script.join(" && ");
            let quoted_agent_cmd: Vec<String> = agent_command.iter().map(|s| {
                if s.contains(' ') || s.contains('"') || s.contains('\'') || s.contains('*') || s.contains('$') {
                    format!("'{}'", s.replace('\'', "'\\''"))
                } else {
                    s.clone()
                }
            }).collect();
            let full_command = format!("{} && {}", joined_script, quoted_agent_cmd.join(" "));
            cmd.arg("sh").arg("-c").arg(&full_command);
        } else {
            cmd.args(agent_command);
        }
    } else {
        cmd.args(agent_command);
    }

    cmd.kill_on_drop(true);

    let timeout_secs = sandbox.timeout_seconds.unwrap_or(120);

    let child_future = cmd.output();
    
    // Set up graceful shutdown channel for SIGTERM / SIGINT
    let (tx, rx) = tokio::sync::oneshot::channel();
    
    tokio::spawn(async move {
        #[cfg(unix)]
        let mut sigterm = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("⚠️ Warning: Failed to register SIGTERM handler: {}", e);
                // Return early from spawn, just disable custom cleanup for SIGTERM
                return;
            }
        };
        
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                let _ = tx.send("SIGINT");
            }
            _ = sigterm.recv() => {
                let _ = tx.send("SIGTERM");
            }
        }

        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            let _ = tx.send("SIGINT");
        }
    });

    let container_name_clone = container_name.clone();
    let engine_clone = engine.clone();

    let run_task = tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async move {
        tokio::select! {
            res = child_future => {
                res
            }
            msg = rx => {
                let sig = msg.unwrap_or("Interrupt");
                println!("\n⚠️ Received {}, forcefully killing orphaned sandbox container ({})...", sig, container_name_clone);
                let _ = tokio::process::Command::new(&engine_clone)
                    .arg("rm")
                    .arg("-f")
                    .arg(&container_name_clone)
                    .output()
                    .await;
                std::process::exit(1);
            }
        }
    });

    match run_task.await {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            Ok((stdout, stderr, output.status.success()))
        },
        Ok(Err(e)) => {
            anyhow::bail!("Failed to execute agent container: {}", e);
        },
        Err(_) => {
            println!("⏱️  Agent execution timed out after {} seconds. Forcefully terminating container ({})...", timeout_secs, container_name);
            let _ = tokio::process::Command::new(&engine)
                .arg("rm")
                .arg("-f")
                .arg(&container_name)
                .output()
                .await;
            
            Ok(("".to_string(), format!("ERROR: Agent execution timed out after {} seconds.", timeout_secs), false))
        }
    }
}
