use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Debug)]
pub struct EpochReport {
    pub run_id: String,
    pub epoch_number: u32,
    pub stdout: String,
    pub stderr: String,
    pub score: f64,
    pub mutations: Vec<String>,
}

pub struct Reporter {
    base_dir: PathBuf,
}

impl Reporter {
    pub fn new() -> Self {
        Self {
            base_dir: PathBuf::from(".neuroplasticity").join("runs"),
        }
    }

    pub fn new_with_base_dir<P: AsRef<Path>>(base_dir: P) -> Self {
        Self {
            base_dir: base_dir.as_ref().to_path_buf(),
        }
    }

    pub fn report_epoch(
        &self,
        run_id: &str,
        epoch_number: u32,
        stdout: &str,
        stderr: &str,
        score: f64,
        mutations: Vec<String>,
    ) -> std::io::Result<()> {
        let run_dir = self.base_dir.join(run_id);
        fs::create_dir_all(&run_dir)?;

        let report = EpochReport {
            run_id: run_id.to_string(),
            epoch_number,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            score,
            mutations,
        };

        let file_path = run_dir.join(format!("epoch-{}.json", epoch_number));
        let json_data = serde_json::to_string_pretty(&report)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        fs::write(file_path, json_data)
    }
}

impl Default for Reporter {
    fn default() -> Self {
        Self::new()
    }
}
