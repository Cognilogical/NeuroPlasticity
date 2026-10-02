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

/// Stable digest of arbitrary text, for patch provenance (F8).
pub fn digest_of(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Everything needed to tell whether a patch still applies to its target (F8).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PatchProvenance {
    pub manifest_hash: String,
    pub evaluator_set_hash: String,
    /// Digest of `target_rules_file` as it stood when the run started.
    pub baseline_rules_digest: String,
    /// Digest of the rules the run produced.
    pub result_rules_digest: String,
    /// Run transcripts, when the agent emitted one (F6a).
    #[serde(default)]
    pub transcript_digest: Option<String>,
    /// Manifest name this patch targets.
    pub target_project: String,
    /// ISO-8601 (UTC, second precision) start of the run.
    pub run_started_at: String,
    /// ISO-8601 (UTC, second precision) end of the run.
    pub run_finished_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DigestDrift {
    Match,
    /// The target's current rules differ from what this patch was derived from.
    Drifted {
        expected: String,
        actual: String,
    },
}

impl DigestDrift {
    pub fn is_match(&self) -> bool {
        matches!(self, DigestDrift::Match)
    }
}

/// Compare a target's current rules against the digest a patch was built from.
///
/// Refusing here is the point: re-verifying a prompt that has drifted reports a
/// result for a different artifact than the one the patch describes.
pub fn check_rules_drift(baseline_digest: &str, current_rules_json: Option<&str>) -> DigestDrift {
    let current = match current_rules_json {
        Some(content) => digest_of(&[content]),
        None => digest_of(&[""]),
    };
    if current == baseline_digest {
        DigestDrift::Match
    } else {
        DigestDrift::Drifted {
            expected: baseline_digest.to_string(),
            actual: current,
        }
    }
}

pub fn calculate_fingerprint(
    agent_command: &[String],
    target_rules_file: &PathBuf,
    manifest_name: &str,
    meta_provider: &str,
    meta_model: &str,
    meta_base_url: &str,
    evaluators_serialized: &str,
    sandbox_serialized: &str,
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
    // The sandbox IS part of the execution: a cached failure from one
    // base_image/timeout must never be replayed against another. (First-run
    // defect 2026-10-01: changing golang:1.23-slim → 1.25-bookworm kept the
    // old fingerprint, replaying a failure the new config could not have.)
    field("sandbox", sandbox_serialized);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_field_delimited() {
        let a = digest_of(&["one", "two"]);
        assert_eq!(a, digest_of(&["one", "two"]));
        // Length-prefixing prevents ["ab","c"] colliding with ["a","bc"].
        assert_ne!(digest_of(&["ab", "c"]), digest_of(&["a", "bc"]));
    }

    #[test]
    fn digest_changes_with_content() {
        assert_ne!(digest_of(&["one"]), digest_of(&["two"]));
    }

    #[test]
    fn matching_rules_report_no_drift() {
        let rules = r#"["a rule"]"#;
        let digest = digest_of(&[rules]);
        assert!(check_rules_drift(&digest, Some(rules)).is_match());
    }

    #[test]
    fn drifted_rules_are_reported_not_reverified() {
        let original = r#"["a rule"]"#;
        let digest = digest_of(&[original]);
        let drifted = check_rules_drift(&digest, Some(r#"["a different rule"]"#));
        assert!(!drifted.is_match());
        match drifted {
            DigestDrift::Drifted { expected, actual } => {
                assert_eq!(expected, digest);
                assert_ne!(actual, digest);
            }
            DigestDrift::Match => unreachable!(),
        }
    }

    /// A rules file that has since been deleted is drift, not a match.
    #[test]
    fn a_missing_rules_file_is_drift() {
        let digest = digest_of(&[r#"["a rule"]"#]);
        assert!(!check_rules_drift(&digest, None).is_match());
    }

    /// The empty baseline is what a run against no rules records, so a target
    /// that has since gained rules must not silently verify.
    #[test]
    fn an_empty_baseline_still_detects_added_rules() {
        let digest = digest_of(&[""]);
        assert!(!check_rules_drift(&digest, Some(r#"["new rule"]"#)).is_match());
    }
}

/// Current UTC time as ISO-8601 with second precision, without pulling in a
/// date-time crate for a patch header.
pub fn now_iso8601() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch -> civil date (Howard Hinnant's algorithm).
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, s)
}

#[cfg(test)]
mod time_tests {
    use super::now_iso8601;

    #[test]
    fn iso8601_is_well_formed_and_utc() {
        let t = now_iso8601();
        assert!(t.ends_with('Z'), "{}", t);
        assert_eq!(t.len(), 20, "{}", t);
        // YYYY-MM-DDTHH:MM:SSZ — strip the zone suffix before checking digits.
        let body = t.trim_end_matches('Z');
        let parts: Vec<&str> = body.split(&['-', 'T', ':'][..]).collect();
        assert_eq!(parts.len(), 6, "{}", t); // YYYY MM DD HH MM SS
        assert!(
            parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())),
            "{}",
            t
        );
    }
}
