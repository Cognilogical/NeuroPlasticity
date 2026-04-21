use std::fs;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;
use anyhow::Result;
use tempfile::TempDir;

pub struct RunnerResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub scratch_dir: TempDir,
}

pub fn setup_workspace(project_dir: &Path) -> Result<TempDir> {
    let scratch_dir = tempfile::Builder::new().prefix("neuroplasticity-run-").tempdir()?;
    let scratch_path = scratch_dir.path();
    copy_dir_filtered(project_dir, scratch_path)?;
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

    anyhow::bail!("Neither podman nor docker was found on this system.\nTo use NeuroPlasticity, please install a container engine:\n\nUbuntu/Debian: sudo apt install podman\nmacOS: brew install podman\nWindows: https://podman.io/getting-started/installation\n\nOr install Docker: https://docs.docker.com/get-docker/");
}

pub fn run_agent(
    scratch_path: &Path,
    engine_preference: &str,
    base_image: &str,
    agent_command: &[String],
) -> Result<(String, String, bool)> {
    let (engine, is_podman) = detect_container_engine(engine_preference)?;
    
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
    cmd.arg("-v");
    cmd.arg(&format!("{}:/workspace:Z", scratch_path.display()));
    cmd.arg("--workdir");
    cmd.arg("/workspace");
    cmd.arg(base_image);
    cmd.args(agent_command);

    let output = cmd.output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok((stdout, stderr, output.status.success()))
}

fn copy_dir_filtered(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in WalkDir::new(src) {
        let entry = entry?;
        let path = entry.path();
        
        let relative_path = path.strip_prefix(src).unwrap();
        
        // Skip root
        if relative_path.as_os_str().is_empty() {
            continue;
        }

        // Check if path contains .git or .neuroplasticity
        let should_ignore = relative_path.components().any(|c| {
            c.as_os_str() == ".git" || c.as_os_str() == ".neuroplasticity"
        });

        if should_ignore {
            continue;
        }

        let target_path = dst.join(relative_path);

        if path.is_dir() {
            fs::create_dir_all(&target_path)?;
        } else if path.is_file() {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(path, &target_path)?;
        }
    }
    Ok(())
}
