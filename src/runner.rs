use std::path::{Path, PathBuf};
use std::process::Command;
use anyhow::Result;
use tempfile::TempDir;
use crate::manifest::Sandbox;

pub struct RunnerResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub scratch_dir: TempDir,
}

pub fn setup_workspace(_project_dir: &Path) -> Result<TempDir> {
    // We no longer copy the entire project directory.
    // Instead we just create an empty scratch directory for the agent to write outputs.
    let scratch_dir = tempfile::Builder::new().prefix("neuroplasticity-run-").tempdir()?;
    Ok(scratch_dir)
}

fn detect_container_engine(preferred: &str) -> Result<(String, bool)> {
    let check_cmd = |cmd: &str| -> bool {
        Command::new(cmd)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };

    if preferred == "docker" && check_cmd("docker") {
        return Ok(("docker".to_string(), false)); // false = is not podman
    }
    
    if preferred == "podman" && check_cmd("podman") {
        return Ok(("podman".to_string(), true)); // true = is podman
    }

    // Fallbacks
    if check_cmd("podman") {
        println!("⚠️  Preferred engine '{}' not found. Falling back to podman.", preferred);
        return Ok(("podman".to_string(), true));
    }
    
    if check_cmd("docker") {
        println!("⚠️  Preferred engine '{}' not found. Falling back to docker.", preferred);
        return Ok(("docker".to_string(), false));
    }

    anyhow::bail!("No container engine (Podman or Docker) found on this system.\nNeuroPlasticity requires Podman to run isolated sandboxes.\n\nPlease install Podman:\nUbuntu/Debian: sudo apt-get install podman\nmacOS: brew install podman\nWindows/Docs: https://podman.io/docs/installation");
}

pub fn run_agent(
    project_dir: &Path,
    scratch_path: &Path,
    sandbox: &Sandbox,
    agent_command: &[String],
) -> Result<(String, String, bool)> {
    let (engine, is_podman) = detect_container_engine(&sandbox.engine)?;
    
    let mut cmd = Command::new(&engine);
    cmd.arg("run");
    cmd.arg("--rm");
    
    if is_podman {
        cmd.arg("--userns=keep-id");
    } else {
        // Fallback for docker on linux
        if let Ok(uid_out) = Command::new("id").arg("-u").output() {
            if let Ok(gid_out) = Command::new("id").arg("-g").output() {
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
    
    // Default Mounts from WorkspaceConfig
    let project_mount = sandbox.workspace.as_ref().map_or("/project", |w| &w.project_mount);
    let scratch_mount = sandbox.workspace.as_ref().map_or("/workspace", |w| &w.scratch_mount);

    // Read-only project mount
    cmd.arg("-v");
    cmd.arg(&format!("{}:{}:ro,Z", project_dir.display(), project_mount));

    // Read-write scratch mount
    cmd.arg("-v");
    cmd.arg(&format!("{}:{}:rw,Z", scratch_path.display(), scratch_mount));

    // Custom Mounts
    if let Some(mounts) = &sandbox.mounts {
        for mount in mounts {
            let expanded_source = shellexpand::tilde(&mount.source).to_string();
            let source_path = PathBuf::from(expanded_source);
            if !source_path.exists() {
                println!("⚠️  Mount source {} does not exist, skipping.", mount.source);
                continue;
            }
            let ro_flag = if mount.readonly { "ro," } else { "" };
            cmd.arg("-v");
            cmd.arg(&format!("{}:{}:{}Z", source_path.display(), mount.target, ro_flag));
        }
    }

    cmd.arg("--workdir");
    cmd.arg(scratch_mount);

    cmd.arg(&sandbox.base_image);

    // Setup script support
    if let Some(setup_script) = &sandbox.setup_script {
        if !setup_script.is_empty() {
            let joined_script = setup_script.join(" && ");
            
            // We need to properly quote the agent_command elements to form a valid shell command
            let quoted_agent_cmd: Vec<String> = agent_command.iter().map(|s| {
                if s.contains(' ') || s.contains('"') || s.contains('\'') || s.contains('*') || s.contains('$') {
                    format!("'{}'", s.replace('\'', "'\\''"))
                } else {
                    s.clone()
                }
            }).collect();
            
            let full_command = format!("{} && {}", joined_script, quoted_agent_cmd.join(" "));
            cmd.arg("sh");
            cmd.arg("-c");
            cmd.arg(&full_command);
        } else {
            cmd.args(agent_command);
        }
    } else {
        cmd.args(agent_command);
    }

    let output = cmd.output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((stdout, stderr, output.status.success()))
}

