use anyhow::Result;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct FingerprintCache {
    // Maps Hash -> (score, stdout, stderr, eval_details_json)
    pub failures: HashMap<String, CachedFailure>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct CachedFailure {
    pub score: f64,
    pub stdout: String,
    pub stderr: String,
}

pub fn get_cache_path() -> PathBuf {
    PathBuf::from(".neuroplasticity/failed_fingerprints.json")
}

pub fn calculate_fingerprint(
    agent_command: &[String],
    target_rules_file: &PathBuf,
    manifest_name: &str,
    meta_provider: &str,
    meta_model: &str,
    meta_base_url: &str,
    evaluators_serialized: &str,
) -> String {
    let mut hasher = Sha256::new();

    // Field boundaries are labelled: concatenating unlabelled values lets two
    // different configs hash identically (provider "ab" + model "c" would
    // otherwise collide with provider "a" + model "bc").
    let mut field = |label: &str, value: &str| {
        hasher.update(format!("|{}={}|", label.len(), label).as_bytes());
        hasher.update(value.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(value.as_bytes());
    };

    // Hash the command
    field("agent_command", &agent_command.join("\u{0}"));

    // Hash the manifest configuration
    field("name", manifest_name);
    field("provider", meta_provider);
    field("model", meta_model);
    // The same model name can resolve to different backends, and a cached
    // failure from one backend must not be replayed against another.
    field("base_url", meta_base_url);
    field("evaluators", evaluators_serialized);

    // Hash the current rules state
    if target_rules_file.exists() {
        if let Ok(content) = std::fs::read_to_string(target_rules_file) {
            // Hash the minified JSON to ignore whitespace formatting differences
            if let Ok(json_arr) = serde_json::from_str::<Vec<String>>(&content) {
                if let Ok(minified) = serde_json::to_string(&json_arr) {
                    hasher.update(minified.as_bytes());
                } else {
                    hasher.update(content.as_bytes());
                }
            } else {
                hasher.update(content.as_bytes());
            }
        }
    }

    // Return hex string of the SHA256 hash
    let result = hasher.finalize();
    hex::encode(result)
}

pub fn check_fingerprint(fingerprint: &str) -> Option<CachedFailure> {
    let path = get_cache_path();
    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(cache) = serde_json::from_str::<FingerprintCache>(&content) {
                return cache.failures.get(fingerprint).cloned();
            }
        }
    }
    None
}

pub fn save_fingerprint(fingerprint: &str, failure: CachedFailure) -> Result<()> {
    let path = get_cache_path();
    let mut cache = if path.exists() {
        let content = fs::read_to_string(&path)?;
        serde_json::from_str::<FingerprintCache>(&content).unwrap_or_default()
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        FingerprintCache::default()
    };

    cache.failures.insert(fingerprint.to_string(), failure);

    let updated_json = serde_json::to_string_pretty(&cache)?;
    fs::write(path, updated_json)?;

    Ok(())
}
