use crate::manifest::Evaluator;
use anyhow::Result;
use std::process::Command;
use std::path::Path;

#[derive(Debug)]
pub struct EvaluatorScore {
    pub name: String,
    pub success: bool,
    pub weight: f64,
}

#[derive(Debug)]
pub struct EvaluationResult {
    pub pass: bool,
    pub score: f64,
    pub total_weight: f64,
    pub passing_weight: f64,
    pub threshold: f64,
    pub details: Vec<EvaluatorScore>,
}

pub fn evaluate(
    evaluators: &[Evaluator],
    working_dir: &Path,
    pass_threshold: f64,
) -> Result<EvaluationResult> {
    let mut total_weight = 0.0;
    let mut passing_weight = 0.0;
    let mut details = Vec::new();

    for eval in evaluators {
        total_weight += eval.weight;

        if eval.script.is_empty() {
            details.push(EvaluatorScore {
                name: eval.name.clone(),
                success: false,
                weight: eval.weight,
            });
            continue;
        }

        let mut cmd = Command::new(&eval.script[0]);
        if eval.script.len() > 1 {
            cmd.args(&eval.script[1..]);
        }
        cmd.current_dir(working_dir);

        let success = match cmd.status() {
            Ok(status) => status.success(),
            Err(_) => false,
        };

        if success {
            passing_weight += eval.weight;
        }

        details.push(EvaluatorScore {
            name: eval.name.clone(),
            success,
            weight: eval.weight,
        });
    }

    let score = if total_weight > 0.0 {
        passing_weight / total_weight
    } else {
        1.0
    };

    Ok(EvaluationResult {
        pass: score >= pass_threshold,
        score,
        total_weight,
        passing_weight,
        threshold: pass_threshold,
        details,
    })
}
