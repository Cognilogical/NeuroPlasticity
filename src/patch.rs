use crate::evaluator::EvaluationResult;

/// The outcome of a single evaluator in one epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct EvaluatorOutcome {
    pub name: String,
    pub success: bool,
    pub weight: f64,
}

impl EvaluatorOutcome {
    pub fn from_result(result: &EvaluationResult) -> Vec<Self> {
        result
            .details
            .iter()
            .map(|d| Self {
                name: d.name.clone(),
                success: d.success,
                weight: d.weight,
            })
            .collect()
    }
}

/// What happened while running one manifest across its epochs.
#[derive(Debug, Clone)]
pub struct ManifestReport {
    pub manifest_name: String,
    pub passed: bool,
    pub epochs_taken: u32,
    /// Captured on the first evaluated epoch, before any rule was mutated.
    pub baseline: Vec<EvaluatorOutcome>,
    /// The last evaluated epoch's results.
    pub final_results: Vec<EvaluatorOutcome>,
}

impl ManifestReport {
    /// Evaluators that passed at baseline and no longer pass.
    ///
    /// A rule that fixes one evaluator by breaking another is a regression, not
    /// progress, and the patch must say so.
    pub fn regressions(&self) -> Vec<&EvaluatorOutcome> {
        self.final_results
            .iter()
            .filter(|current| {
                !current.success
                    && self
                        .baseline
                        .iter()
                        .any(|b| b.name == current.name && b.success)
            })
            .collect()
    }

    /// Evaluators that went from failing at baseline to passing.
    pub fn improvements(&self) -> Vec<&EvaluatorOutcome> {
        self.final_results
            .iter()
            .filter(|current| {
                current.success
                    && self
                        .baseline
                        .iter()
                        .any(|b| b.name == current.name && !b.success)
            })
            .collect()
    }
}

/// Overall Waterfall outcome, which determines what the patch may claim.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    /// Every manifest passed, no evaluator regressed.
    Verified,
    /// A manifest exhausted its epochs. Its rules are a proposal, not a fix.
    Partial { manifest: String, epochs: u32 },
    /// The run ended with at least one previously-passing evaluator failing.
    Regressed {
        manifest: String,
        evaluators: Vec<String>,
    },
}

impl RunOutcome {
    /// A stable, machine-readable token for this outcome.
    ///
    /// Consumers should filter on this rather than matching prose in the status
    /// line, which is written for humans and may be reworded.
    pub fn token(&self) -> &'static str {
        match self {
            RunOutcome::Verified => "verified",
            RunOutcome::Partial { .. } => "partial",
            RunOutcome::Regressed { .. } => "regressed",
        }
    }

    /// Whether the emitted rules may be treated as verified improvements.
    pub fn rules_are_verified(&self) -> bool {
        self.is_verified()
    }

    /// The status line for the patch header.
    ///
    /// This must be a function of the result. A constant here is how a failed
    /// run ends up claiming its rules were verified.
    pub fn status_line(&self) -> String {
        match self {
            RunOutcome::Verified => {
                "**Status:** ✅ Verified against deterministic evaluators across the entire Waterfall.\n".to_string()
            }
            RunOutcome::Partial { manifest, epochs } => format!(
                "**Status:** ⚠️ PARTIAL — manifest `{manifest}` did not pass after {epochs} epoch(s). \
                 The rules below are **UNVERIFIED** — treat them as a proposal, not a fix.\n"
            ),
            RunOutcome::Regressed {
                manifest,
                evaluators,
            } => format!(
                "**Status:** ⚠️ REGRESSED — manifest `{manifest}` finished with evaluator(s) that passed at \
                 baseline and now fail: {}. See the evaluator delta table. Do not apply without review.\n",
                evaluators.join(", ")
            ),
        }
    }

    /// Whether the rules may be presented as verified improvements.
    pub fn is_verified(&self) -> bool {
        matches!(self, RunOutcome::Verified)
    }

    /// Advice about applying the rules. Only earned by a verified run, and
    /// only meaningful when there are rules to apply.
    pub fn application_guidance(&self, rule_count: usize) -> &'static str {
        if rule_count == 0 {
            return "";
        }
        if self.is_verified() {
            "The following behavioral constraints successfully corrected the agent's failure paths. \
             You should permanently inject these into the target agent's system prompt or `AGENTS.md`:\n"
        } else {
            "The rules below were produced by an automated loop that did **not** converge. Review each one \
             against the failing evidence in the per-epoch reports before adopting it. Do not inject them \
             wholesale, and do not describe them as verified:\n"
        }
    }
}

/// How one evaluator moved between the baseline and the final epoch.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Unchanged,
    Improved,
    Regressed,
    /// Present in one set but not the other; nothing to compare against.
    Uncomparable,
}

impl Delta {
    pub fn label(&self) -> &'static str {
        match self {
            Delta::Unchanged => "—",
            Delta::Improved => "improved",
            Delta::Regressed => "**REGRESSION**",
            Delta::Uncomparable => "n/a",
        }
    }
}

/// Compare baseline to final results for one evaluator.
pub fn delta_for(
    name: &str,
    baseline: &[EvaluatorOutcome],
    final_results: &[EvaluatorOutcome],
) -> Delta {
    let before = baseline.iter().find(|o| o.name == name);
    let after = final_results.iter().find(|o| o.name == name);

    match (before, after) {
        (Some(b), Some(a)) => match (b.success, a.success) {
            (true, true) => Delta::Unchanged,
            (false, true) => Delta::Improved,
            (false, false) => Delta::Unchanged,
            (true, false) => Delta::Regressed,
        },
        _ => Delta::Uncomparable,
    }
}

/// Render the evaluator delta table. Present whenever a baseline was captured,
/// so a reader can see movement rather than trusting a summary claim.
pub fn render_delta_table(report: &ManifestReport) -> String {
    let mut names: Vec<&str> = report
        .baseline
        .iter()
        .chain(report.final_results.iter())
        .map(|o| o.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut out = String::from("\n### Evaluator delta (baseline → final)\n\n");
    out.push_str("| evaluator | baseline | final | delta |\n");
    out.push_str("|---|---|---|---|\n");

    for name in names {
        let before = report
            .baseline
            .iter()
            .find(|o| o.name == name)
            .map(|o| if o.success { "PASS" } else { "FAIL" })
            .unwrap_or("—");
        let after = report
            .final_results
            .iter()
            .find(|o| o.name == name)
            .map(|o| if o.success { "PASS" } else { "FAIL" })
            .unwrap_or("—");
        out.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            name,
            before,
            after,
            delta_for(name, &report.baseline, &report.final_results).label()
        ));
    }

    out
}

/// Compose the patch document.
pub fn render_patch(
    target_project: &str,
    outcome: &RunOutcome,
    report: Option<&ManifestReport>,
    rules: &[String],
) -> String {
    let mut doc = String::from("# 🧠 NeuroPlasticity Improvement Patch\n\n");
    doc.push_str(&format!("**Target Project:** `{}`\n", target_project));
    doc.push_str(&outcome.status_line());
    doc.push('\n');

    // Machine-readable header so a consumer can filter without parsing prose
    // (F0 item 4). The prose above is for humans and may be reworded.
    doc.push_str("<!-- neuroplasticity:status\n");
    doc.push_str(&format!("outcome: {}\n", outcome.token()));
    doc.push_str(&format!(
        "rules_verified: {}\n",
        outcome.rules_are_verified()
    ));
    if let RunOutcome::Partial { manifest, epochs } = outcome {
        doc.push_str(&format!("failed_manifest: {}\n", manifest));
        doc.push_str(&format!("epochs: {}\n", epochs));
    }
    if let RunOutcome::Regressed {
        manifest,
        evaluators,
    } = outcome
    {
        doc.push_str(&format!("failed_manifest: {}\n", manifest));
        for evaluator in evaluators {
            doc.push_str(&format!("regressed_evaluator: {}\n", evaluator));
        }
    }
    if let Some(report) = report {
        for name in report
            .regressions()
            .into_iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>()
        {
            doc.push_str(&format!("regressed_evaluator: {}\n", name));
        }
    }
    doc.push_str("-->\n\n");

    let guidance = outcome.application_guidance(rules.len());
    if !guidance.is_empty() {
        doc.push_str(guidance);
        doc.push('\n');
    }

    if let Some(report) = report {
        doc.push_str(&render_delta_table(report));
        doc.push('\n');
    }

    doc.push_str(if outcome.is_verified() {
        "### Verified rules\n\n"
    } else {
        "### Proposed rules\n\n"
    });
    for (i, rule) in rules.iter().enumerate() {
        doc.push_str(&format!(
            "#### Rule {}\n<!-- verified:{} -->\n> {}\n\n",
            i + 1,
            outcome.rules_are_verified(),
            rule
        ));
    }

    doc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &str, success: bool) -> EvaluatorOutcome {
        EvaluatorOutcome {
            name: name.to_string(),
            success,
            weight: 1.0,
        }
    }

    fn sample_report() -> ManifestReport {
        ManifestReport {
            manifest_name: "demo".to_string(),
            passed: true,
            epochs_taken: 2,
            baseline: vec![
                outcome("jq: schema valid", true),
                outcome("llm: tone is warm", false),
                outcome("host_bash: no secrets", true),
            ],
            final_results: vec![
                outcome("jq: schema valid", true),
                outcome("llm: tone is warm", true),
                outcome("host_bash: no secrets", false),
            ],
        }
    }

    // --- F0 ---

    /// The status line must be derived from the outcome. Against the previous
    /// implementation, which hardcoded a `✅ Verified` literal, this fails.
    #[test]
    fn partial_status_is_not_claimed_as_verified() {
        let outcome = RunOutcome::Partial {
            manifest: "demo".to_string(),
            epochs: 3,
        };
        let status = outcome.status_line();
        assert!(status.contains("PARTIAL"), "{}", status);
        assert!(status.contains("UNVERIFIED"), "{}", status);
        assert!(
            !status.contains("✅"),
            "a failing run must not claim ✅: {}",
            status
        );
        assert!(!outcome.is_verified());
    }

    #[test]
    fn regressed_status_names_the_broken_evaluators() {
        let outcome = RunOutcome::Regressed {
            manifest: "demo".to_string(),
            evaluators: vec!["host_bash: no secrets".to_string()],
        };
        let status = outcome.status_line();
        assert!(status.contains("REGRESSED"), "{}", status);
        assert!(status.contains("host_bash: no secrets"), "{}", status);
        assert!(!status.contains("✅"), "{}", status);
    }

    /// The happy path must not change gratuitously.
    #[test]
    fn verified_status_keeps_original_wording() {
        assert_eq!(
            RunOutcome::Verified.status_line(),
            "**Status:** ✅ Verified against deterministic evaluators across the entire Waterfall.\n"
        );
        assert!(RunOutcome::Verified.is_verified());
    }

    /// A patch from a failing run must not tell a reader to inject the rules.
    #[test]
    fn unverified_runs_withhold_the_inject_instruction() {
        for outcome in [
            RunOutcome::Partial {
                manifest: "demo".to_string(),
                epochs: 2,
            },
            RunOutcome::Regressed {
                manifest: "demo".to_string(),
                evaluators: vec!["e".to_string()],
            },
        ] {
            let guidance = outcome.application_guidance(1);
            assert!(
                !guidance.contains("permanently inject"),
                "unearned advice in guidance: {}",
                guidance
            );
        }
        assert!(
            RunOutcome::Verified
                .application_guidance(1)
                .contains("permanently inject")
        );
    }

    /// The artifact itself must carry the warning, since the artifact is what
    /// gets handed to another agent and stdout is usually lost.
    #[test]
    fn patch_artifact_carries_the_warning() {
        let patch = render_patch(
            "demo",
            &RunOutcome::Partial {
                manifest: "demo".to_string(),
                epochs: 3,
            },
            None,
            &["Do not wrap JSON in fences".to_string()],
        );
        assert!(patch.contains("PARTIAL"), "{}", patch);
        assert!(!patch.contains("permanently inject"), "{}", patch);
    }

    /// A consumer must be able to filter on the outcome without reading prose.
    #[test]
    fn patch_carries_a_machine_readable_status() {
        let partial = render_patch(
            "demo",
            &RunOutcome::Partial {
                manifest: "demo".to_string(),
                epochs: 2,
            },
            None,
            &["rule one".to_string()],
        );
        assert!(partial.contains("outcome: partial"), "{}", partial);
        assert!(partial.contains("rules_verified: false"), "{}", partial);
        assert!(partial.contains("failed_manifest: demo"), "{}", partial);
        assert!(partial.contains("<!-- verified:false -->"), "{}", partial);

        let regressed = render_patch(
            "demo",
            &RunOutcome::Regressed {
                manifest: "demo".to_string(),
                evaluators: vec!["host_bash: no secrets".to_string()],
            },
            Some(&sample_report()),
            &["rule one".to_string()],
        );
        assert!(regressed.contains("outcome: regressed"), "{}", regressed);
        assert!(regressed.contains("rules_verified: false"), "{}", regressed);

        let verified = render_patch(
            "demo",
            &RunOutcome::Verified,
            None,
            &["rule one".to_string()],
        );
        assert!(verified.contains("outcome: verified"), "{}", verified);
        assert!(verified.contains("rules_verified: true"), "{}", verified);
        assert!(verified.contains("<!-- verified:true -->"), "{}", verified);
    }

    /// Regression names are machine-readable even when the outcome summary is
    /// the more general Partial variant.
    #[test]
    fn partial_outcome_still_lists_regressed_evaluators() {
        let patch = render_patch(
            "demo",
            &RunOutcome::Partial {
                manifest: "demo".to_string(),
                epochs: 2,
            },
            Some(&sample_report()),
            &["rule".to_string()],
        );
        assert!(
            patch.contains("regressed_evaluator: host_bash: no secrets"),
            "{}",
            patch
        );
    }

    /// The delta table and the rule list must be separated, or a consumer that
    /// splits sections on `###` reads the last table row as if it were a rule.
    #[test]
    fn rules_are_not_parsed_out_of_the_delta_table() {
        let patch = render_patch(
            "demo",
            &RunOutcome::Verified,
            Some(&sample_report()),
            &["The only real rule.".to_string()],
        );

        // Every `###` heading must be a real section, never a table row.
        let sections: Vec<&str> = patch
            .lines()
            .filter_map(|line| line.strip_prefix("### "))
            .collect();
        assert!(
            sections.iter().all(|s| !s.contains('|')),
            "a delta-table row leaked into the rule headings: {:?}",
            sections
        );
        assert!(patch.contains("#### Rule 1"));
        assert!(patch.contains("> The only real rule."));
    }

    #[test]
    fn verified_runs_label_the_section_as_verified() {
        let verified = render_patch("d", &RunOutcome::Verified, None, &["r".to_string()]);
        assert!(verified.contains("### Verified rules"), "{}", verified);
        assert!(!verified.contains("### Proposed rules"), "{}", verified);

        let partial = render_patch(
            "d",
            &RunOutcome::Partial {
                manifest: "d".to_string(),
                epochs: 1,
            },
            None,
            &["r".to_string()],
        );
        assert!(partial.contains("### Proposed rules"), "{}", partial);
        assert!(!partial.contains("### Verified rules"), "{}", partial);
    }

    /// An empty rule set must not carry the "inject these" advice: there is
    /// nothing to inject, and the phrasing implies otherwise.
    #[test]
    fn no_rules_means_nothing_to_apply() {
        let patch = render_patch("d", &RunOutcome::Verified, None, &[]);
        assert!(!patch.contains("permanently inject"), "{}", patch);
        assert!(!patch.contains("#### Rule"), "{}", patch);
    }

    // --- F1 ---

    /// The case that currently hides: the run-level weighted score still
    /// clears `pass_threshold` because the weights line up, even though an
    /// evaluator that passed at baseline now fails.
    ///
    /// Weighted score here is 0.9 (one 1.0-weight failure out of 10.0 total),
    /// which clears a 0.8 threshold — yet a green evaluator regressed.
    #[test]
    fn regression_is_caught_even_when_the_run_score_passes() {
        let report = ManifestReport {
            manifest_name: "demo".to_string(),
            passed: true, // the run passed overall
            epochs_taken: 2,
            baseline: vec![
                EvaluatorOutcome {
                    name: "big".to_string(),
                    success: false,
                    weight: 9.0,
                },
                EvaluatorOutcome {
                    name: "small".to_string(),
                    success: true,
                    weight: 1.0,
                },
            ],
            final_results: vec![
                EvaluatorOutcome {
                    name: "big".to_string(),
                    success: true,
                    weight: 9.0,
                },
                EvaluatorOutcome {
                    name: "small".to_string(),
                    success: false,
                    weight: 1.0,
                },
            ],
        };

        // The run-level weighted score passes...
        let passing_weight = 9.0;
        let total_weight = 10.0;
        let score = passing_weight / total_weight;
        assert!(score >= 0.8, "precondition: score {} should pass", score);

        // ...but a previously-passing evaluator regressed, so the patch must
        // not be presented as verified.
        assert_eq!(report.regressions().len(), 1);
        assert_eq!(report.regressions()[0].name, "small");
        assert!(report.passed, "the run itself reported success");

        let patch = render_patch(
            "demo",
            &RunOutcome::Regressed {
                manifest: "demo".to_string(),
                evaluators: vec!["small".to_string()],
            },
            Some(&report),
            &["broaden the rule".to_string()],
        );
        assert!(patch.contains("outcome: regressed"), "{}", patch);
        assert!(patch.contains("rules_verified: false"), "{}", patch);
        assert!(!patch.contains("permanently inject"), "{}", patch);
    }

    #[test]
    fn detects_a_regression() {
        let report = sample_report();
        let regressions = report.regressions();
        assert_eq!(regressions.len(), 1);
        assert_eq!(regressions[0].name, "host_bash: no secrets");
    }

    #[test]
    fn detects_improvements() {
        let report = sample_report();
        let improvements = report.improvements();
        assert_eq!(improvements.len(), 1);
        assert_eq!(improvements[0].name, "llm: tone is warm");
    }

    #[test]
    fn baseline_with_no_movement_has_no_regressions() {
        let report = ManifestReport {
            final_results: vec![outcome("a", true), outcome("b", false)],
            ..sample_report()
        };
        let regressions = report.regressions();
        assert!(regressions.is_empty(), "{:?}", regressions);
    }

    #[test]
    fn delta_table_marks_the_regression() {
        let patch = render_patch(
            "demo",
            &RunOutcome::Regressed {
                manifest: "demo".to_string(),
                evaluators: vec!["host_bash: no secrets".to_string()],
            },
            Some(&sample_report()),
            &["be careful".to_string()],
        );
        assert!(
            patch.contains("| `host_bash: no secrets` | PASS | FAIL | **REGRESSION** |"),
            "{}",
            patch
        );
        assert!(
            patch.contains("| `llm: tone is warm` | FAIL | PASS | improved |"),
            "{}",
            patch
        );
        assert!(
            patch.contains("| `jq: schema valid` | PASS | PASS | — |"),
            "{}",
            patch
        );
    }

    /// An evaluator that fails at baseline and still fails is not an improvement.
    #[test]
    fn still_failing_is_not_an_improvement() {
        assert_eq!(
            delta_for("x", &[outcome("x", false)], &[outcome("x", false)]),
            Delta::Unchanged
        );
    }

    #[test]
    fn evaluator_absent_from_baseline_is_uncomparable() {
        assert_eq!(
            delta_for("new", &[], &[outcome("new", true)]),
            Delta::Uncomparable
        );
    }
}
