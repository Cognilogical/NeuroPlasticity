use std::fs;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;
use anyhow::Result;

pub struct RunnerResult {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

pub fn run_agent(
    project_dir: &Path,
    base_image: &str,
    agent_command: &[&str],
) -> Result<RunnerResult> {
    // Create a temporary scratch directory
    let scratch_dir = tempfile::Builder::new().prefix("neuroplasticity-run-").tempdir()?;
    let scratch_path = scratch_dir.path();

    // Copy the current repository into the scratch dir, ignoring .git and .neuroplasticity
    copy_dir_filtered(project_dir, scratch_path)?;

    // Construct and execute the Podman command:
    // podman run --rm --userns=keep-id --security-opt no-new-privileges -v <scratch>:/workspace:Z --workdir /workspace <base_image> <agent_command>
    let mut cmd = Command::new("podman");
    cmd.args(&[
        "run",
        "--rm",
        "--userns=keep-id",
        "--security-opt",
        "no-new-privileges",
        "-v",
        &format!("{}:/workspace:Z", scratch_path.display()),
        "--workdir",
        "/workspace",
        base_image,
    ]);
    cmd.args(agent_command);

    let output = cmd.output()?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    Ok(RunnerResult {
        stdout,
        stderr,
        success: output.status.success(),
    })
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
