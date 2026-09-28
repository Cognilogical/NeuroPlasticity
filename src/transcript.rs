use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default sidecar filename an agent writes in the scratch workspace.
///
/// JSON Lines so an agent can append incrementally without buffering, and so a
/// truncated final line degrades to "steps up to here" rather than failing the
/// whole parse.
pub const TRANSCRIPT_FILENAME: &str = "transcript.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    #[default]
    Ok,
    Failed,
    Skipped,
}

impl StepStatus {
    pub fn is_failure(self) -> bool {
        matches!(self, StepStatus::Failed)
    }

    pub fn label(self) -> &'static str {
        match self {
            StepStatus::Ok => "ok",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
        }
    }
}

/// One step in a run, with a stable id so a failure can be attributed to it.
///
/// This is the ordered model F5 and F6 were missing: without it, "the step that
/// failed" is unrepresentable and per-unit assertions have to parse strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// Stable identifier, unique within a transcript. Referenced by an
    /// evaluator's `unit` and by an invariant's attribution.
    pub id: String,
    /// Zero-based position. Assigned on parse so an agent need not count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    /// What kind of step this was (`lookup`, `book`, `tool_call`, ...).
    #[serde(default)]
    pub kind: String,
    /// Free-form reference to what this step consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<String>,
    /// Free-form reference to what this step produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<String>,
    #[serde(default)]
    pub status: StepStatus,
    /// Human-readable detail, surfaced to the optimizer with a localized
    /// failure so it can write a narrow rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An ordered run transcript.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub steps: Vec<Step>,
}

impl Transcript {
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Steps that failed, in order.
    pub fn failing_steps(&self) -> Vec<&Step> {
        self.steps
            .iter()
            .filter(|s| s.status.is_failure())
            .collect()
    }

    /// The first failing step, which is the smallest unit a rule can be written
    /// against (F5).
    pub fn first_failure(&self) -> Option<&Step> {
        self.steps.iter().find(|s| s.status.is_failure())
    }

    /// Look up a step by stable id.
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Every step of a given kind, for cross-cutting invariants (F6).
    pub fn steps_of_kind(&self, kind: &str) -> Vec<&Step> {
        self.steps.iter().filter(|s| s.kind == kind).collect()
    }

    /// A stable digest of the transcript, for patch provenance (F8).
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for step in &self.steps {
            hasher.update(step.id.as_bytes());
            hasher.update(b"|");
            hasher.update(step.kind.as_bytes());
            hasher.update(b"|");
            hasher.update(step.status.label().as_bytes());
            hasher.update(b"\n");
        }
        hex::encode(hasher.finalize())
    }

    /// Render for the optimizer: the failing step plus a little surrounding
    /// context, labelled so a rule is phrased narrowly (F5).
    pub fn render_localized_failure(&self, unit: Option<&str>) -> Option<String> {
        let target = match unit {
            Some(id) => self.step(id).or_else(|| self.first_failure()),
            None => self.first_failure(),
        }?;
        let index = target.index.unwrap_or(0);
        // One step of context on either side is enough to stay coherent
        // without handing back the whole run.
        let from = index.saturating_sub(1);
        let to = (index + 2).min(self.steps.len());

        let mut out = String::new();
        out.push_str(&format!(
            "Failing unit: `{}` (step {}, kind={}, status={})\n",
            target.id,
            index,
            target.kind,
            target.status.label()
        ));
        if let Some(detail) = &target.detail {
            out.push_str(&format!("Detail: {}\n", detail));
        }
        if let Some(output) = &target.output_ref {
            out.push_str(&format!("Output: {}\n", output));
        }
        out.push_str("Surrounding steps:\n");
        for step in &self.steps[from..to] {
            let marker = if step.id == target.id { ">>" } else { "  " };
            out.push_str(&format!(
                "{} `{}` — {} ({})\n",
                marker,
                step.id,
                step.kind,
                step.status.label()
            ));
        }
        Some(out)
    }

    /// Human-readable form for the patch artifact.
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} step(s)\n\n", self.steps.len()));
        for step in &self.steps {
            let index = step.index.unwrap_or(0);
            let marker = if step.status.is_failure() {
                "❌"
            } else {
                "  "
            };
            out.push_str(&format!(
                "{marker} `{}` ({}): {} — {}\n",
                step.id,
                step.kind,
                index,
                step.status.label()
            ));
        }
        out
    }
}

/// Parse a JSON-lines transcript.
///
/// Tolerates a truncated final line: an agent killed mid-write should still
/// yield the steps it completed rather than failing the run.
pub fn parse_jsonl(content: &str) -> Result<Transcript> {
    let mut steps = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Step>(trimmed) {
            Ok(mut step) => {
                step.index = Some(steps.len());
                steps.push(step);
            }
            // Only tolerate a bad *last* line (a partial write); anything else
            // is a real authoring error worth surfacing.
            Err(e) => {
                let is_last =
                    content.lines().filter(|l| !l.trim().is_empty()).count() == line_no + 1;
                if is_last {
                    eprintln!(
                        "⚠️  Ignoring truncated final transcript line {} ({}).",
                        line_no + 1,
                        e
                    );
                    break;
                }
                return Err(anyhow::anyhow!(
                    "transcript line {} is not a valid step: {}",
                    line_no + 1,
                    e
                ));
            }
        }
    }
    Ok(Transcript { steps })
}

/// Locate and read the sidecar an agent may have written.
///
/// Returns `None` when absent or empty: a missing transcript degrades to
/// today's behavior and never fails a run.
pub fn read_sidecar(workspace: &Path) -> Option<Transcript> {
    let path: PathBuf = workspace.join(TRANSCRIPT_FILENAME);
    if !path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    match parse_jsonl(&content) {
        Ok(t) if t.is_empty() => None,
        Ok(t) => Some(t),
        Err(e) => {
            eprintln!(
                "⚠️  Ignoring unusable transcript at {:?}: {}. Continuing without it.",
                path, e
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(id: &str, kind: &str, status: StepStatus) -> Step {
        Step {
            id: id.to_string(),
            index: None,
            kind: kind.to_string(),
            input_ref: None,
            output_ref: None,
            status,
            detail: None,
        }
    }

    const SAMPLE: &str = concat!(
        r#"{"id":"s1","kind":"lookup","status":"ok","output_ref":"flight-123"}"#,
        "\n",
        r#"{"id":"s2","kind":"book","status":"failed","detail":"no availability"}"#,
        "\n",
        r#"{"id":"s3","kind":"notify","status":"ok"}"#,
    );

    #[test]
    fn parses_jsonl_in_order() {
        let t = parse_jsonl(SAMPLE).unwrap();
        assert_eq!(t.len(), 3);
        // Index is assigned on parse, so an agent need not count.
        assert_eq!(t.steps[0].index, Some(0));
        assert_eq!(t.steps[2].index, Some(2));
        assert_eq!(t.step("s2").unwrap().kind, "book");
    }

    #[test]
    fn identifies_the_failing_step() {
        let t = parse_jsonl(SAMPLE).unwrap();
        let failures = t.failing_steps();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].id, "s2");
        assert_eq!(t.first_failure().unwrap().id, "s2");
    }

    #[test]
    fn localizes_a_failure_to_its_unit_with_context() {
        let t = parse_jsonl(SAMPLE).unwrap();
        let rendered = t.render_localized_failure(None).unwrap();
        assert!(rendered.contains("Failing unit: `s2`"), "{}", rendered);
        assert!(rendered.contains("no availability"), "{}", rendered);
        // Neighbouring steps are included so a rule can stay coherent.
        assert!(rendered.contains("`s1`"), "{}", rendered);
        assert!(rendered.contains("`s3`"), "{}", rendered);
    }

    /// A rule written against a whole-run log is necessarily global. The
    /// localized rendering must lead with the unit so the model scopes to it.
    #[test]
    fn localization_labels_the_unit_before_the_detail() {
        let t = parse_jsonl(SAMPLE).unwrap();
        let rendered = t.render_localized_failure(None).unwrap();
        let unit_at = rendered.find("Failing unit").unwrap();
        let detail_at = rendered.find("Detail").unwrap();
        assert!(unit_at < detail_at, "{}", rendered);
    }

    #[test]
    fn a_named_unit_selects_that_step() {
        let t = parse_jsonl(SAMPLE).unwrap();
        let rendered = t.render_localized_failure(Some("s3")).unwrap();
        assert!(rendered.contains("Failing unit: `s3`"), "{}", rendered);
    }

    #[test]
    fn a_missing_unit_falls_back_to_the_first_failure() {
        let t = parse_jsonl(SAMPLE).unwrap();
        let rendered = t.render_localized_failure(Some("does-not-exist")).unwrap();
        assert!(rendered.contains("Failing unit: `s2`"), "{}", rendered);
    }

    #[test]
    fn an_unknown_unit_with_no_failure_yields_nothing() {
        let t = parse_jsonl(concat!(r#"{"id":"s1","kind":"lookup","status":"ok"}"#)).unwrap();
        assert!(t.render_localized_failure(Some("nope")).is_none());
    }

    #[test]
    fn a_truncated_final_line_degrades_instead_of_failing() {
        // An agent killed mid-write leaves a partial line.
        let truncated = format!("{}\n{{\"id\":\"s3\",\"kind\":\"noti", SAMPLE.trim_end());
        let t = parse_jsonl(&truncated).unwrap();
        assert_eq!(t.len(), 3, "the three good steps should survive");
    }

    #[test]
    fn a_corrupt_middle_line_is_an_error() {
        let corrupt = concat!(
            "not json at all\n",
            r#"{"id":"s2","kind":"book","status":"ok"}"#,
        );
        assert!(
            parse_jsonl(corrupt).is_err(),
            "a bad first line must not be ignored"
        );
    }

    #[test]
    fn empty_input_yields_an_empty_transcript() {
        let t = parse_jsonl("").unwrap();
        assert!(t.is_empty());
        assert!(t.first_failure().is_none());
    }

    #[test]
    fn missing_sidecar_degrades_to_none() {
        let dir = std::env::temp_dir().join("np-transcript-test-missing");
        let _ = std::fs::create_dir_all(&dir);
        assert!(read_sidecar(&dir).is_none());
    }

    #[test]
    fn digest_changes_when_the_run_changes() {
        let a = parse_jsonl(SAMPLE).unwrap();
        let b = parse_jsonl(concat!(
            r#"{"id":"s1","kind":"lookup","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"book","status":"ok"}"#
        ))
        .unwrap();
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn digest_ignores_optional_fields() {
        // Only identity/kind/status should define a run's shape.
        let a = parse_jsonl(concat!(r#"{"id":"s1","kind":"lookup","status":"ok"}"#)).unwrap();
        let b = parse_jsonl(concat!(
            r#"{"id":"s1","kind":"lookup","status":"ok","detail":"noise","output_ref":"x"}"#
        ))
        .unwrap();
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn steps_of_kind_supports_invariant_checks() {
        let t = parse_jsonl(concat!(
            r#"{"id":"s1","kind":"book","status":"ok"}"#,
            "\n",
            r#"{"id":"s2","kind":"lookup","status":"failed"}"#,
            "\n",
            r#"{"id":"s3","kind":"book","status":"ok"}"#
        ))
        .unwrap();
        assert_eq!(t.steps_of_kind("book").len(), 2);
        // F6's example: a booking after a failed lookup.
        let first_failure = t.first_failure().unwrap();
        let bookings_after: Vec<&Step> = t
            .steps_of_kind("book")
            .into_iter()
            .filter(|s| s.index > first_failure.index)
            .collect();
        assert_eq!(bookings_after.len(), 1);
        assert_eq!(bookings_after[0].id, "s3");
    }
}
