use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use anyhow::Result;
use std::hash::{Hash, Hasher};
use std::collections::hash_map::DefaultHasher;

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
    meta_model: &str,
    evaluators_serialized: &str,
) -> String {
    let mut hasher = DefaultHasher::new();
    
    // Hash the command
    for part in agent_command {
        part.hash(&mut hasher);
    }
    
    // Hash the manifest name
    manifest_name.hash(&mut hasher);

    // Hash the meta optimizer model
    meta_model.hash(&mut hasher);

    // Hash the evaluators (so if tests change, cache invalidates)
    evaluators_serialized.hash(&mut hasher);

    // Hash the current injected rules if they exist
    if target_rules_file.exists() {
        if let Ok(content) = fs::read_to_string(target_rules_file) {
            // If the content is valid JSON, parse it and serialize it deterministically
            if let Ok(json_val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Ok(deterministic_json) = serde_json::to_string(&json_val) {
                    deterministic_json.hash(&mut hasher);
                } else {
                    content.hash(&mut hasher);
                }
            } else {
                content.hash(&mut hasher);
            }
        }
    }
    
    format!("{:x}", hasher.finish())
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
