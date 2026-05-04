use anyhow::{Context, Result};
use tokio::process::Command;
use std::process::Stdio;

/// Executes a shell command to check if a binary exists in the system PATH
pub async fn check_cmd(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Detects the available container engine (podman or docker)
/// Returns a tuple of (engine_name, is_podman)
pub async fn detect_container_engine(preferred: &Option<String>) -> Result<(String, bool)> {
    if let Some(engine) = preferred {
        if check_cmd(engine).await {
            let is_podman = engine == "podman";
            return Ok((engine.clone(), is_podman));
        }
    }

    if check_cmd("podman").await {
        return Ok(("podman".to_string(), true));
    }
    
    if check_cmd("docker").await {
        return Ok(("docker".to_string(), false));
    }

    anyhow::bail!("No container engine (Podman or Docker) found on this system.\nNeuroPlasticity requires an engine to run isolated sandboxes.\n\nPlease install Podman:\nUbuntu/Debian: sudo apt-get install podman\nmacOS: brew install podman\nWindows/Docs: https://podman.io/docs/installation");
}
