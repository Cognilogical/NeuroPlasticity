//! TypeSafe System One ("Jev") — the judgment-model evaluator provider.
//!
//! Why this is its own module and not a branch in `llm_client`: Jev is a
//! *judgment* model, not a chat model. It receives state plus a typed
//! question and returns schema-constrained answers with probabilities —
//! never prose. Routing that through chat-shaped completion plumbing would
//! rebuild the exact parse-failure class this provider exists to eliminate
//! ("replied `**PASS** — valid JSON`" cannot happen when the answer is typed
//! at the API boundary), and would lose batched typed judgments. Shared
//! concerns — the cloud semaphore, egress, fingerprinting, quorum roles —
//! live in their existing homes; only the request builder and response
//! decoding are new (design §Implementation shape).
//!
//! API reference: <https://docs.typesafe.ai/api> (verified 2026-10-02).

use crate::evaluator::Verdict;
use crate::manifest::{TypesafeQuestion, TypesafeResolved};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// POST `{base_url}/v1/systemone`.
///
/// The host is never hardcoded: the owner's free tier is served behind a
/// different base URL (the OpenCode Zen gateway) with the same request
/// shape, so the doc's request builder must not pin the host.
const ENDPOINT_PATH: &str = "/v1/systemone";

/// Per-attempt HTTP deadline, matching `llm_client::ATTEMPT_TIMEOUT`.
/// async reqwest has no default timeout, and nothing else wraps this call —
/// without it a wedged endpoint would hang the epoch forever.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Up to 3 attempts with exponential backoff. 429 and 529 are **expected**:
/// TypeSafe rate limits "adjusting dynamically", so they are retryable
/// conditions, not fatal ones (design §Fallback). 401/403/422 abort on the
/// first response — retrying a configuration error changes nothing.
const MAX_ATTEMPTS: u32 = 3;

/// Record/replay cassette directory. When set, a request whose recorded
/// response exists is answered from disk without touching the network, and a
/// missing recording is fetched live and written back. When unset, every
/// call is live. This is what keeps `cargo test` free and deterministic
/// (design §Record/replay: REQUIRED, not optional).
pub const CASSETTE_ENV: &str = "NEUROPLASTICITY_TYPESAFE_CASSETTES";

/// Build the full endpoint URL for a base URL, tolerating a trailing slash.
pub fn endpoint(base_url: &str) -> String {
    format!("{}{}", base_url.trim_end_matches('/'), ENDPOINT_PATH)
}

/// One judgment request, fully resolved (defaults already applied by
/// `manifest::TypesafeResolved`).
pub struct TypesafeCall<'a> {
    pub base_url: &'a str,
    pub model: &'a str,
    pub api_key_env: &'a str,
    /// The state text (rendered document).
    pub state: &'a str,
    /// Questions with their text already rendered against run state.
    pub questions: &'a BTreeMap<String, TypesafeQuestion>,
}

/// Build the System One request body (verified shape, docs §Request body).
///
/// `criteria` is omitted when absent rather than sent as `null`: the API
/// distinguishes "no criteria" from a malformed one, and a manifest that
/// declares no rubric must not be rewritten into a 422.
pub fn build_request_body(
    state: &str,
    model: &str,
    questions: &BTreeMap<String, TypesafeQuestion>,
) -> serde_json::Value {
    let mut entries = serde_json::Map::new();
    for (id, question) in questions {
        let mut entry = serde_json::json!({
            "type": question.primitive.label(),
            "instructions": question.question,
        });
        if let Some(criteria) = &question.criteria {
            entry["criteria"] = criteria.clone();
        }
        entries.insert(id.clone(), entry);
    }
    serde_json::json!({
        "state": state,
        "model": model,
        "questions": entries,
    })
}

/// Token usage for one request, as reported by the API.
///
/// Telemetry rather than verdict data: a gateway that omits it must not
/// abort a run whose answers parsed cleanly, hence the default.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TypesafeUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// One typed answer. Tagged by the response's own `type` field, so a reply
/// whose shape does not match any primitive fails to decode — a decoding
/// failure is retried, never scored as an answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypesafeAnswer {
    /// Probability the condition holds, 0–1. Carries **no** confidence
    /// field (docs: "Noul answers don't carry one").
    Noul { noul: f64 },
    /// The chosen option, its full distribution, and a 0–1 confidence
    /// derived from that distribution's shape.
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    /// A probability-weighted position across ordered levels (can land
    /// between levels), with legend, distribution, and confidence.
    Score {
        score: f64,
        legend: BTreeMap<String, String>,
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

/// A successful response body (docs §Response body).
#[derive(Debug, Clone, Deserialize)]
pub struct TypesafeResponse {
    /// The model that **actually answered** — the resolved version, which
    /// may differ from the requested one (e.g. an alias). Recorded in
    /// verdict provenance so a decision is re-verified against what ran.
    pub model: String,
    /// One answer per question, keyed by the question id asked.
    pub answers: BTreeMap<String, TypesafeAnswer>,
    #[serde(default)]
    pub usage: TypesafeUsage,
}

/// Render a `{{key}}` template against `subs`.
///
/// Fail loud by contract: an unresolved placeholder is a configuration
/// error, never passed through as literal braces — the grader would
/// silently answer a question the author did not write. Substituted values
/// are not re-scanned, so a transcript containing `{{…}}` cannot loop.
pub fn render_template(tmpl: &str, subs: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::with_capacity(tmpl.len());
    let mut rest = tmpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(format!(
                "unclosed template placeholder starting at \"{{{{{}\"",
                truncate(after, 40)
            ));
        };
        let key = after[..end].trim();
        match subs.iter().find(|(name, _)| *name == key) {
            Some((_, value)) => out.push_str(value),
            None => {
                let available = if subs.is_empty() {
                    "none were available this run".to_string()
                } else {
                    subs.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
                };
                return Err(format!(
                    "unresolved template placeholder '{{{{{}}}}}' — no value was available for \
                     '{}' this run (provided: {})",
                    key, key, available
                ));
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Answer → verdict mapping (pure, unit-tested; design §Proposed manifest shape)
// ---------------------------------------------------------------------------

/// Noul: probability ≥ `pass_above` → PASS, ≤ `fail_below` → FAIL, in
/// between → INDETERMINATE.
///
/// `indeterminate_below` deliberately does **not** apply here: Noul answers
/// carry no confidence field (verified from docs), so the dead-band between
/// the two thresholds IS the uncertainty signal — p near 0.5 is the model
/// saying "unsure", and an unsure grader must not invent a failure.
pub fn verdict_for_noul(p: f64, spec: &TypesafeResolved) -> Verdict {
    if p >= spec.pass_above {
        Verdict::Pass
    } else if p <= spec.fail_below {
        Verdict::Fail
    } else {
        Verdict::Indeterminate
    }
}

/// Choice: route through `verdict_by_answer`, then apply the confidence
/// floor.
///
/// Answers outside the routing map are INDETERMINATE (safer than guessing —
/// the map may intentionally cover only a subset). Choice answers *do*
/// carry a confidence: distribution shape catches what the point answer
/// misses, e.g. two options nearly tied.
pub fn verdict_for_choice(choice: &str, confidence: f64, spec: &TypesafeResolved) -> Verdict {
    let routed = spec
        .verdict_by_answer
        .as_ref()
        .and_then(|map| map.get(choice))
        .and_then(|label| Verdict::parse(label))
        .unwrap_or(Verdict::Indeterminate);
    if confidence < spec.indeterminate_below {
        Verdict::Indeterminate
    } else {
        routed
    }
}

/// Score: normalize the probability-weighted score over its levels
/// (`score / (levels - 1)`), then apply the same bands as a Noul — plus the
/// same confidence floor as Choice, since Score answers carry confidence
/// too (e.g. a score torn between adjacent levels).
pub fn verdict_for_score(
    score: f64,
    levels: usize,
    confidence: f64,
    spec: &TypesafeResolved,
) -> Verdict {
    let top = levels.saturating_sub(1) as f64;
    if top <= 0.0 {
        // Fewer than two levels cannot be normalized. Undecidable, and an
        // undecidable judgment is INDETERMINATE — never a guess.
        return Verdict::Indeterminate;
    }
    if confidence < spec.indeterminate_below {
        return Verdict::Indeterminate;
    }
    let normalized = (score / top).clamp(0.0, 1.0);
    verdict_for_noul(normalized, spec)
}

/// Map one typed answer onto a verdict using its question's configuration.
pub fn verdict_for_answer(
    answer: &TypesafeAnswer,
    question: &TypesafeQuestion,
    spec: &TypesafeResolved,
) -> Verdict {
    match answer {
        TypesafeAnswer::Noul { noul } => verdict_for_noul(*noul, spec),
        TypesafeAnswer::Choice {
            choice, confidence, ..
        } => verdict_for_choice(choice, *confidence, spec),
        TypesafeAnswer::Score {
            score, confidence, ..
        } => {
            let levels = question
                .criteria
                .as_ref()
                .and_then(|c| c.as_array())
                .map_or(0, |a| a.len());
            verdict_for_score(*score, levels, *confidence, spec)
        }
    }
}

/// Combine per-question verdicts into one. Every question is a
/// requirement, so the combination is a conjunction: ALL Pass → Pass,
/// ANY Fail → Fail, else INDETERMINATE. (An empty set judges nothing, so
/// it is INDETERMINATE rather than a vacuous Pass.)
pub fn combine_verdicts(verdicts: &[Verdict]) -> Verdict {
    if verdicts.is_empty() {
        return Verdict::Indeterminate;
    }
    if verdicts.contains(&Verdict::Fail) {
        return Verdict::Fail;
    }
    if verdicts.iter().all(|v| *v == Verdict::Pass) {
        return Verdict::Pass;
    }
    Verdict::Indeterminate
}

/// A one-line factual description of an answer, for the per-question
/// detail in the evaluator report. Values, not interpretations: the
/// verdict derivation is unit-tested separately.
pub fn answer_detail(answer: &TypesafeAnswer) -> String {
    match answer {
        TypesafeAnswer::Noul { noul } => format!("noul p={:.2}", noul),
        TypesafeAnswer::Choice {
            choice, confidence, ..
        } => format!("choice {:?} conf={:.2}", choice, confidence),
        TypesafeAnswer::Score {
            score, confidence, ..
        } => format!("score={:.2} conf={:.2}", score, confidence),
    }
}

/// Synthesize an `llm`-style grader prompt from TypeSafe questions.
///
/// Used by the load-time `fallback: "llm"` transform and for chat-model
/// graders inside a TypeSafe quorum: the chat model is asked the same
/// questions and must reply with the single PASS/FAIL/INDETERMINATE JSON
/// object the `llm` evaluator expects, combining them by the same
/// conjunction rule `combine_verdicts` applies. Question texts are taken
/// verbatim — template placeholders resolve against run state, which does
/// not exist at load time.
pub fn synthesize_llm_prompt(questions: &BTreeMap<String, TypesafeQuestion>) -> String {
    let mut out = String::from(
        "You are an automated evaluator. Judge the target document against EVERY question \
         below, then answer with a single JSON object: {\"verdict\": \"PASS\"|\"FAIL\"|\
         \"INDETERMINATE\", \"reason\": \"<one sentence>\"} and nothing else.\n\
         Combine the per-question judgments as a conjunction: answer PASS only if every \
         question judges PASS; answer FAIL if any question judges FAIL; otherwise answer \
         INDETERMINATE.\n\
         Use INDETERMINATE only when a question cannot be judged from what is shown — never \
         as a substitute for FAIL when you can tell.\n\nQuestions:\n",
    );
    if questions.is_empty() {
        out.push_str("(no questions were declared — this grader cannot decide)\n");
        return out;
    }
    for (id, question) in questions {
        out.push_str(&format!(
            "- [{}] ({}) {}\n",
            id,
            question.primitive.label(),
            question.question
        ));
        if let Some(criteria) = &question.criteria {
            out.push_str(&format!("  Criteria: {}\n", criteria));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Record / replay cassettes
// ---------------------------------------------------------------------------

/// One recorded exchange. `response` holds the raw body verbatim, so replay
/// exercises the same decoder a live call would.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cassette {
    /// The model the request asked for (the response's own `model` records
    /// what actually answered).
    pub request_model: String,
    pub response: String,
}

/// The cassette directory, when record/replay mode is enabled.
fn cassette_dir() -> Option<PathBuf> {
    let raw = std::env::var(CASSETTE_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

/// Where the cassette for one exact request lives.
///
/// The key is a length-delimited hash of (base_url, model, canonical JSON
/// body): changing the endpoint, the pinned model, the question text, the
/// criteria, or the state all invalidate the recording — the same rule the
/// failure fingerprint uses (a changed grader config is a different grader).
/// `serde_json`'s default map ordering makes the serialization canonical,
/// so key construction is order-independent.
pub fn cassette_file(dir: &Path, base_url: &str, model: &str, body: &serde_json::Value) -> PathBuf {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(body).unwrap_or_default();
    let mut hasher = Sha256::new();
    for part in [base_url, model, canonical.as_str()] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
    }
    dir.join(format!("{}.json", hex::encode(hasher.finalize())))
}

// ---------------------------------------------------------------------------
// The call
// ---------------------------------------------------------------------------

/// Execute one judgment request: replay from a cassette when one exists,
/// otherwise call the endpoint (with retries) and record the exchange.
///
/// Ordering matters. Replay is checked **before** the API key, so recorded
/// fixtures need no credential and no network — that is what makes the test
/// suite free and offline. The key is resolved before any network I/O, so a
/// missing credential fails immediately with a clear message instead of a
/// confusing 401 after a timeout.
pub async fn run(call: &TypesafeCall<'_>) -> Result<TypesafeResponse> {
    let body = build_request_body(call.state, call.model, call.questions);
    let url = endpoint(call.base_url);

    if let Some(dir) = cassette_dir() {
        let path = cassette_file(&dir, call.base_url, call.model, &body);
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read TypeSafe cassette {:?}", path))?;
            let cassette: Cassette = serde_json::from_str(&raw)
                .with_context(|| format!("TypeSafe cassette {:?} is not valid JSON", path))?;
            return serde_json::from_str::<TypesafeResponse>(&cassette.response).with_context(
                || {
                    format!(
                        "TypeSafe cassette {:?} holds an unusable response body",
                        path
                    )
                },
            );
        }
        // A miss falls through to a live call; the response is written back
        // below so the next run replays it (record mode, design §Record/replay).
    }

    let token = crate::llm_client::resolve_api_key(call.api_key_env, &url).with_context(|| {
        format!(
            "TypeSafe API key could not be resolved from `{}`",
            call.api_key_env
        )
    })?;

    let (response, raw_body) =
        post_with_retries(&url, token.as_deref(), &body, call.api_key_env).await?;

    if let Some(dir) = cassette_dir() {
        let path = cassette_file(&dir, call.base_url, call.model, &body);
        if let Err(e) = record_cassette(&path, call.model, &raw_body) {
            // Not fatal — the live call itself succeeded — but loud: a lost
            // recording means the next test run would spend real money.
            eprintln!(
                "⚠️  Failed to record TypeSafe cassette {:?}: {} — the next run will call the \
                 live API again.",
                path, e
            );
        }
    }

    Ok(response)
}

/// Write one cassette. Failures bubble up so `run` can warn loudly.
fn record_cassette(path: &Path, request_model: &str, response_body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cassette directory {:?}", parent))?;
    }
    let cassette = Cassette {
        request_model: request_model.to_string(),
        response: response_body.to_string(),
    };
    let json = serde_json::to_string_pretty(&cassette)?;
    std::fs::write(path, json).with_context(|| format!("Failed to write {:?}", path))?;
    Ok(())
}

/// POST the request, retrying transient conditions with exponential
/// backoff. Returns the parsed response plus the raw body (for cassettes).
async fn post_with_retries(
    url: &str,
    token: Option<&str>,
    body: &serde_json::Value,
    api_key_env: &str,
) -> Result<(TypesafeResponse, String)> {
    let client = reqwest::Client::builder()
        .timeout(ATTEMPT_TIMEOUT)
        .build()
        .context("Failed to build the TypeSafe HTTP client")?;

    let mut last_error = "request was not attempted".to_string();

    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let backoff_ms = 500u64.saturating_mul(1 << (attempt - 2));
            eprintln!(
                "⏳ Retrying TypeSafe request in {}ms (attempt {}/{})...",
                backoff_ms, attempt, MAX_ATTEMPTS
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

        let mut request = client.post(url).json(body);
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            request = request.bearer_auth(token);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                // Connect failures and timeouts are transient by nature.
                last_error = format!("connection to {} failed: {}", url, e);
                continue;
            }
        };

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // See `is_retryable`: a rate limit is a scheduling event, a
            // configuration error is not worth a second attempt.
            let retryable = is_retryable(status.as_u16());
            last_error = format!(
                "HTTP {} from {} — {}{}",
                status,
                url,
                truncate(&text, 500),
                if retryable {
                    String::new()
                } else {
                    format!(
                        " (not retried: {})",
                        non_retryable_reason(status.as_u16(), api_key_env)
                    )
                }
            );
            if !retryable {
                break;
            }
            continue;
        }

        match serde_json::from_str::<TypesafeResponse>(&text) {
            Ok(parsed) => return Ok((parsed, text)),
            Err(e) => {
                // Typed decoding is the whole point of this provider: a body
                // that does not decode is never scored as an answer. Malformed
                // success bodies are usually transient, so retry.
                last_error = format!(
                    "response was not a valid TypeSafe body: {} — {}",
                    e,
                    truncate(&text, 300)
                );
                continue;
            }
        }
    }

    // Self-contained message: callers print errors with `{}`, so the status
    // detail must live here rather than in a context chain that would hide it.
    Err(anyhow::anyhow!(
        "TypeSafe request to {} failed after {} attempt(s): {}",
        url,
        MAX_ATTEMPTS,
        last_error
    ))
}

/// Whether a status deserves another attempt: a rate limit (429) and any
/// server-side failure (500-599, which includes TypeSafe's own 529) are
/// transient. Everything else — 400/401/403/404/422 — returns the same way on
/// a second try, so burning attempts would only delay the loud error message.
fn is_retryable(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// Why a status is not worth retrying, phrased as the actual remedy.
fn non_retryable_reason(status: u16, api_key_env: &str) -> String {
    match status {
        401 | 403 => format!(
            "authentication failed — check the API key in `{}`",
            api_key_env
        ),
        422 => "the request body was rejected — fix the manifest's questions/document".to_string(),
        _ => "the status is not transient".to_string(),
    }
}

/// Truncate on a character boundary so multibyte bodies can't panic.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect::<String>() + "…"
}

/// Serializes tests that read or write these process-wide environment
/// variables. `std::env::set_var` is unsafe in edition 2024 precisely
/// because env mutation races with other threads; taking this lock is the
/// safety invariant — every test in this crate that touches them holds it.
///
/// A Tokio mutex rather than a `std` one, because the lock has to stay held
/// for the whole replay: another test repointing `CASSETTE_ENV` while an
/// async call is reading it back would be the same race this exists to
/// prevent, and only Tokio's guard is designed to span an `await`.
#[cfg(test)]
pub(crate) fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
        once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));
    &LOCK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{TypesafePrimitive, TypesafeQuestion};

    fn question(primitive: TypesafePrimitive, text: &str) -> TypesafeQuestion {
        TypesafeQuestion {
            primitive,
            question: text.to_string(),
            criteria: None,
        }
    }

    fn resolved() -> TypesafeResolved {
        TypesafeResolved {
            document: None,
            questions: BTreeMap::new(),
            pass_above: 0.75,
            fail_below: 0.35,
            verdict_by_answer: None,
            indeterminate_below: 0.5,
            base_url: "https://api.typesafe.ai".to_string(),
            model: "jev-1.13.0".to_string(),
            api_key_env: "TYPESAFE_API_KEY".to_string(),
        }
    }

    fn with_routing(answers: &[(&str, &str)]) -> TypesafeResolved {
        let mut spec = resolved();
        spec.verdict_by_answer = Some(
            answers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        spec
    }

    // --- Template rendering ---

    #[test]
    fn renders_every_substitution_key() {
        let out = render_template(
            "Transcript:\n{{run.transcript}}\nFile:\n{{target_file}}",
            &[
                ("run.transcript", "{\"id\":\"s1\"}"),
                ("target_file", "body {{not a key}}"),
            ],
        )
        .unwrap();
        assert_eq!(
            out,
            "Transcript:\n{\"id\":\"s1\"}\nFile:\nbody {{not a key}}"
        );
    }

    #[test]
    fn substitutes_repeated_keys_and_passes_plain_text_through() {
        assert_eq!(
            render_template("plain text", &[("a", "x")]).unwrap(),
            "plain text"
        );
        assert_eq!(
            render_template("{{a}}-{{a}}", &[("a", "1")]).unwrap(),
            "1-1"
        );
        // Whitespace inside the braces is tolerated.
        assert_eq!(render_template("{{ a }}", &[("a", "1")]).unwrap(), "1");
    }

    /// An unresolved placeholder is a config error, not text to pass along:
    /// literal braces reaching the grader would silently ask a different
    /// question than the author wrote.
    #[test]
    fn an_unresolved_placeholder_is_an_error() {
        let err = render_template("Hello {{rule.text}}", &[("run.transcript", "t")]).unwrap_err();
        assert!(err.contains("rule.text"), "{}", err);
        assert!(
            err.contains("run.transcript"),
            "must list what was provided: {}",
            err
        );

        let err = render_template("{{x}}", &[]).unwrap_err();
        assert!(err.contains("none were available"), "{}", err);
    }

    #[test]
    fn an_unclosed_placeholder_is_an_error() {
        let err = render_template("broken {{key", &[]).unwrap_err();
        assert!(err.contains("unclosed"), "{}", err);
    }

    // --- Request building ---

    #[test]
    fn builds_the_documented_request_shape() {
        let mut questions = BTreeMap::new();
        let mut noul = question(TypesafePrimitive::Noul, "Does it comply?");
        noul.criteria = Some(serde_json::json!({"true": "complies", "false": "does not"}));
        questions.insert("passes".to_string(), noul);
        questions.insert(
            "missing".to_string(),
            question(TypesafePrimitive::Noul, "Any gaps?"),
        );

        let body = build_request_body("STATE", "jev-1.13.0", &questions);

        assert_eq!(body["state"], "STATE");
        assert_eq!(body["model"], "jev-1.13.0");
        assert_eq!(body["questions"]["passes"]["type"], "noul");
        assert_eq!(
            body["questions"]["passes"]["instructions"],
            "Does it comply?"
        );
        assert_eq!(body["questions"]["passes"]["criteria"]["true"], "complies");
        // `criteria` is omitted entirely when the manifest declares none —
        // not sent as null, which the API would treat as a malformed rubric.
        assert!(
            body["questions"]["missing"].get("criteria").is_none(),
            "{}",
            body
        );
        assert_eq!(body["questions"]["missing"]["instructions"], "Any gaps?");
    }

    #[test]
    fn request_uses_the_primitive_wire_names() {
        let mut questions = BTreeMap::new();
        questions.insert("c".to_string(), question(TypesafePrimitive::Choice, "pick"));
        questions.insert("s".to_string(), question(TypesafePrimitive::Score, "rate"));
        let body = build_request_body("S", "m", &questions);
        assert_eq!(body["questions"]["c"]["type"], "choice");
        assert_eq!(body["questions"]["s"]["type"], "score");
    }

    // --- Response parsing ---

    #[test]
    fn parses_all_three_answer_types() {
        let raw = r#"{
            "model": "jev-1.13.7",
            "answers": {
                "n": { "type": "noul", "noul": 0.95 },
                "c": { "type": "choice", "choice": "billing",
                       "probabilities": { "billing": 0.88, "technical": 0.12 },
                       "confidence": 0.81 },
                "s": { "type": "score", "score": 1.05,
                       "legend": { "0": "Calm", "1": "Frustrated", "2": "Very angry" },
                       "probabilities": { "0": 0.0, "1": 0.95, "2": 0.05 },
                       "confidence": 0.92 }
            },
            "usage": { "input_tokens": 304, "output_tokens": 18 }
        }"#;
        let resp: TypesafeResponse = serde_json::from_str(raw).unwrap();
        // The RESOLVED version that answered — recorded in provenance.
        assert_eq!(resp.model, "jev-1.13.7");
        assert_eq!(resp.usage.input_tokens, 304);

        match &resp.answers["n"] {
            TypesafeAnswer::Noul { noul } => assert_eq!(*noul, 0.95),
            other => panic!("wrong variant: {:?}", other),
        }
        match &resp.answers["c"] {
            TypesafeAnswer::Choice {
                choice,
                confidence,
                probabilities,
            } => {
                assert_eq!(choice, "billing");
                assert_eq!(*confidence, 0.81);
                assert_eq!(probabilities["technical"], 0.12);
            }
            other => panic!("wrong variant: {:?}", other),
        }
        match &resp.answers["s"] {
            TypesafeAnswer::Score {
                score,
                confidence,
                legend,
                ..
            } => {
                assert_eq!(*score, 1.05);
                assert_eq!(*confidence, 0.92);
                assert_eq!(legend["2"], "Very angry");
            }
            other => panic!("wrong variant: {:?}", other),
        }
    }

    /// A gateway that omits usage must not abort a run whose answers parsed.
    #[test]
    fn usage_is_optional_but_answers_and_model_are_not() {
        let resp: TypesafeResponse = serde_json::from_str(r#"{"model":"m","answers":{}}"#).unwrap();
        assert_eq!(resp.usage.input_tokens, 0);
        assert!(
            serde_json::from_str::<TypesafeResponse>(r#"{"answers":{}}"#).is_err(),
            "a response without a resolved model cannot be attributed"
        );
        assert!(
            serde_json::from_str::<TypesafeResponse>(r#"{"model":"m"}"#).is_err(),
            "answers are the whole point"
        );
    }

    /// An answer whose type matches no primitive fails to decode rather
    /// than being coerced into a verdict.
    #[test]
    fn an_unknown_answer_type_does_not_decode() {
        let raw = r#"{"model":"m","answers":{"x":{"type":"prose","text":"PASS"}}}"#;
        assert!(serde_json::from_str::<TypesafeResponse>(raw).is_err());
    }

    // --- Verdict mapping: Noul bands ---

    #[test]
    fn noul_bands_map_probability_to_verdicts() {
        let spec = resolved();
        assert_eq!(verdict_for_noul(0.80, &spec), Verdict::Pass);
        assert_eq!(
            verdict_for_noul(0.75, &spec),
            Verdict::Pass,
            "boundary is inclusive"
        );
        assert_eq!(verdict_for_noul(0.30, &spec), Verdict::Fail);
        assert_eq!(
            verdict_for_noul(0.35, &spec),
            Verdict::Fail,
            "boundary is inclusive"
        );
        // The dead-band: uncertain, not failed.
        assert_eq!(verdict_for_noul(0.50, &spec), Verdict::Indeterminate);
        assert_eq!(verdict_for_noul(0.74, &spec), Verdict::Indeterminate);
        assert_eq!(verdict_for_noul(0.36, &spec), Verdict::Indeterminate);
    }

    /// The dead-band IS the uncertainty signal for Noul: with no confidence
    /// field to compare, an aggressive `indeterminate_below` must not
    /// override a decisive probability.
    #[test]
    fn indeterminate_below_does_not_apply_to_noul() {
        let mut spec = resolved();
        spec.indeterminate_below = 0.99;
        assert_eq!(verdict_for_noul(0.90, &spec), Verdict::Pass);
        assert_eq!(verdict_for_noul(0.10, &spec), Verdict::Fail);
        // Still undecidable in the dead-band, floor or not.
        assert_eq!(verdict_for_noul(0.50, &spec), Verdict::Indeterminate);
    }

    // --- Verdict mapping: Choice ---

    #[test]
    fn choice_routes_through_verdict_by_answer() {
        let spec = with_routing(&[("benign", "PASS"), ("broken", "FAIL")]);
        assert_eq!(verdict_for_choice("benign", 0.9, &spec), Verdict::Pass);
        assert_eq!(verdict_for_choice("broken", 0.9, &spec), Verdict::Fail);
    }

    /// Unmapped answers are INDETERMINATE — the map may cover only a
    /// subset, and guessing is not an option.
    #[test]
    fn an_unmapped_choice_answer_is_indeterminate() {
        let spec = with_routing(&[("benign", "PASS")]);
        assert_eq!(
            verdict_for_choice("broken", 0.9, &spec),
            Verdict::Indeterminate
        );
        let unmapped = resolved();
        assert_eq!(
            verdict_for_choice("anything", 0.9, &unmapped),
            Verdict::Indeterminate
        );
    }

    /// Distribution shape catches what the point answer misses: a routed
    /// PASS with low confidence is still INDETERMINATE.
    #[test]
    fn choice_confidence_floor_forces_indeterminate() {
        let spec = with_routing(&[("benign", "PASS")]);
        assert_eq!(
            verdict_for_choice("benign", 0.49, &spec),
            Verdict::Indeterminate
        );
        assert_eq!(
            verdict_for_choice("benign", 0.50, &spec),
            Verdict::Pass,
            "floor is exclusive below"
        );
    }

    // --- Verdict mapping: Score ---

    #[test]
    fn score_is_normalized_over_its_levels() {
        let spec = resolved();
        // 3 levels → top is 2, so score 1.0 normalizes to 0.5 (dead-band).
        assert_eq!(verdict_for_score(2.0, 3, 0.9, &spec), Verdict::Pass);
        assert_eq!(
            verdict_for_score(1.6, 3, 0.9, &spec),
            Verdict::Pass,
            "1.6/2 = 0.8 ≥ 0.75"
        );
        assert_eq!(
            verdict_for_score(1.0, 3, 0.9, &spec),
            Verdict::Indeterminate
        );
        assert_eq!(
            verdict_for_score(1.2, 3, 0.9, &spec),
            Verdict::Indeterminate,
            "1.2/2 = 0.6 sits inside the dead band"
        );
        assert_eq!(
            verdict_for_score(0.6, 3, 0.9, &spec),
            Verdict::Fail,
            "0.6/2 = 0.3 is at or below fail_below (0.35), not in the dead band"
        );
        assert_eq!(verdict_for_score(0.0, 3, 0.9, &spec), Verdict::Fail);
        // Scores can land between (or, per docs, slightly beyond) levels.
        assert_eq!(
            verdict_for_score(1.05, 3, 0.9, &spec),
            Verdict::Indeterminate
        );
    }

    #[test]
    fn score_applies_the_confidence_floor() {
        let spec = resolved();
        // A top score the model is unsure about is not evidence of a pass.
        assert_eq!(
            verdict_for_score(2.0, 3, 0.4, &spec),
            Verdict::Indeterminate
        );
        assert_eq!(
            verdict_for_score(0.0, 3, 0.4, &spec),
            Verdict::Indeterminate
        );
    }

    #[test]
    fn a_score_without_levels_is_indeterminate_not_a_guess() {
        let spec = resolved();
        assert_eq!(
            verdict_for_score(1.0, 0, 0.9, &spec),
            Verdict::Indeterminate
        );
        assert_eq!(
            verdict_for_score(1.0, 1, 0.9, &spec),
            Verdict::Indeterminate
        );
    }

    // --- Verdict mapping: dispatch and combination ---

    #[test]
    fn verdict_for_answer_dispatches_on_the_typed_answer() {
        let mut spec = resolved();
        spec.verdict_by_answer = Some(
            [("broken".to_string(), "FAIL".to_string())]
                .into_iter()
                .collect(),
        );
        let choice_q = {
            let mut q = question(TypesafePrimitive::Choice, "which?");
            q.criteria = Some(serde_json::json!({"broken": null, "benign": null}));
            q
        };
        let score_q = {
            let mut q = question(TypesafePrimitive::Score, "rate");
            q.criteria = Some(serde_json::json!(["low", "mid", "high"]));
            q
        };

        assert_eq!(
            verdict_for_answer(
                &TypesafeAnswer::Noul { noul: 0.9 },
                &question(TypesafePrimitive::Noul, "ok?"),
                &spec
            ),
            Verdict::Pass
        );
        assert_eq!(
            verdict_for_answer(
                &TypesafeAnswer::Choice {
                    choice: "broken".to_string(),
                    probabilities: BTreeMap::new(),
                    confidence: 0.9,
                },
                &choice_q,
                &spec
            ),
            Verdict::Fail
        );
        assert_eq!(
            verdict_for_answer(
                &TypesafeAnswer::Score {
                    score: 2.0,
                    legend: BTreeMap::new(),
                    probabilities: BTreeMap::new(),
                    confidence: 0.9,
                },
                &score_q,
                &spec
            ),
            Verdict::Pass
        );
    }

    /// The conjunction: every question is a requirement.
    #[test]
    fn combining_verdicts_is_a_conjunction() {
        use Verdict::{Fail, Indeterminate, Pass};
        assert_eq!(combine_verdicts(&[Pass, Pass]), Pass);
        assert_eq!(combine_verdicts(&[Pass, Fail]), Fail, "any failure fails");
        assert_eq!(combine_verdicts(&[Fail, Indeterminate]), Fail);
        assert_eq!(combine_verdicts(&[Pass, Indeterminate]), Indeterminate);
        assert_eq!(combine_verdicts(&[Indeterminate]), Indeterminate);
        assert_eq!(
            combine_verdicts(&[]),
            Indeterminate,
            "nothing judged must never vacuously pass"
        );
    }

    // --- Fallback prompt synthesis ---

    #[test]
    fn synthesized_prompt_carries_questions_criteria_and_the_contract() {
        let mut questions = BTreeMap::new();
        let mut noul = question(TypesafePrimitive::Noul, "Does it comply?");
        noul.criteria = Some(serde_json::json!({"true": "complies"}));
        questions.insert("passes".to_string(), noul);
        questions.insert(
            "kind".to_string(),
            TypesafeQuestion {
                primitive: TypesafePrimitive::Choice,
                question: "Which class?".to_string(),
                criteria: Some(serde_json::json!({"benign": null})),
            },
        );

        let prompt = synthesize_llm_prompt(&questions);
        assert!(prompt.contains("Does it comply?"), "{}", prompt);
        assert!(prompt.contains("Which class?"), "{}", prompt);
        assert!(prompt.contains("complies"), "{}", prompt);
        assert!(prompt.contains("PASS"), "{}", prompt);
        assert!(prompt.contains("INDETERMINATE"), "{}", prompt);
        assert!(
            prompt.contains("conjunction"),
            "must state the combination rule: {}",
            prompt
        );
    }

    // --- Cassettes ---

    /// Replay: a recorded exchange is answered from disk with no API key
    /// and no network. The base URL points at a host that cannot resolve,
    /// so any attempt to go live would fail the test.
    #[tokio::test]
    async fn a_recorded_request_replays_without_the_network() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = env_lock().lock().await;

        let mut questions = BTreeMap::new();
        questions.insert(
            "passes".to_string(),
            question(TypesafePrimitive::Noul, "Does it comply?"),
        );
        let base_url = "https://typesafe-replay.invalid";
        let model = "jev-1.13.0";
        let state = "the state under test";

        let body = build_request_body(state, model, &questions);
        let recorded = r#"{
            "model": "jev-1.13.7",
            "answers": { "passes": { "type": "noul", "noul": 0.9 } },
            "usage": { "input_tokens": 10, "output_tokens": 2 }
        }"#;
        let path = cassette_file(dir.path(), base_url, model, &body);
        record_cassette(&path, model, recorded).expect("write fixture");

        unsafe {
            // SAFETY: held under env_lock(), the crate-wide invariant for
            // tests that mutate these variables.
            std::env::set_var(CASSETTE_ENV, dir.path());
        }

        let call = TypesafeCall {
            base_url,
            model,
            api_key_env: "NEUROPLASTICITY_TEST_UNSET_API_KEY",
            state,
            questions: &questions,
        };
        let resp = run(&call).await.expect("replay must succeed without a key");
        assert_eq!(
            resp.model, "jev-1.13.7",
            "the resolved model is what replayed"
        );
        match &resp.answers["passes"] {
            TypesafeAnswer::Noul { noul } => assert_eq!(*noul, 0.9),
            other => panic!("unexpected answer: {:?}", other),
        }
    }

    /// With record mode on, a miss falls through to key resolution — and a
    /// missing key fails immediately with a clear message, before any
    /// network I/O (the endpoint would not resolve from the test sandbox
    /// anyway, so this also proves ordering).
    #[tokio::test]
    async fn a_missing_key_with_no_cassette_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = env_lock().lock().await;

        unsafe {
            // SAFETY: held under env_lock().
            std::env::set_var(CASSETTE_ENV, dir.path());
            std::env::remove_var("NEUROPLASTICITY_TEST_MISSING_API_KEY");
        }

        let mut questions = BTreeMap::new();
        questions.insert("p".to_string(), question(TypesafePrimitive::Noul, "ok?"));
        let call = TypesafeCall {
            base_url: "https://api.typesafe.ai",
            model: "jev-1.13.0",
            api_key_env: "NEUROPLASTICITY_TEST_MISSING_API_KEY",
            state: "s",
            questions: &questions,
        };
        let err = run(&call).await.unwrap_err().to_string();
        assert!(
            err.contains("NEUROPLASTICITY_TEST_MISSING_API_KEY"),
            "must name the variable to set: {}",
            err
        );
    }

    /// A manifest naming a hostile env var is rejected by the shared guard —
    /// TypeSafe reads keys through the same validation as the llm path.
    #[tokio::test]
    async fn a_hostile_api_key_env_is_rejected_before_any_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _guard = env_lock().lock().await;
        unsafe {
            // SAFETY: held under env_lock().
            std::env::set_var(CASSETTE_ENV, dir.path());
        }

        let mut questions = BTreeMap::new();
        questions.insert("p".to_string(), question(TypesafePrimitive::Noul, "ok?"));
        let call = TypesafeCall {
            base_url: "https://api.typesafe.ai",
            model: "jev-1.13.0",
            api_key_env: "SSH_PRIVATE_KEY",
            state: "s",
            questions: &questions,
        };
        // `{:#}` is how the evaluator prints it, so the assertion covers the
        // whole chain — the context names the variable, the cause names the
        // security rule that rejected it.
        let err = format!("{:#}", run(&call).await.unwrap_err());
        assert!(err.contains("Security Exception"), "{}", err);
        assert!(err.contains("SSH_PRIVATE_KEY"), "{}", err);
    }

    /// The cassette key must move when the request moves: a reworded
    /// question, a different model, or a different endpoint all miss the
    /// recording (the same invalidation rule as the failure fingerprint).
    #[test]
    fn cassette_keys_track_the_grader_configuration() {
        let dir = Path::new("/cassettes");
        let mut questions = BTreeMap::new();
        questions.insert("p".to_string(), question(TypesafePrimitive::Noul, "ok?"));
        let body = build_request_body("s", "jev-1.13.0", &questions);
        let key = cassette_file(dir, "https://api.typesafe.ai", "jev-1.13.0", &body);

        let mut reworded = BTreeMap::new();
        reworded.insert(
            "p".to_string(),
            question(TypesafePrimitive::Noul, "is it ok?"),
        );
        let body2 = build_request_body("s", "jev-1.13.0", &reworded);
        assert_ne!(
            key,
            cassette_file(dir, "https://api.typesafe.ai", "jev-1.13.0", &body2)
        );

        assert_ne!(
            key,
            cassette_file(dir, "https://zen.example", "jev-1.13.0", &body)
        );
        assert_ne!(
            key,
            cassette_file(dir, "https://api.typesafe.ai", "jev-2.0.0", &body)
        );

        // Same request, same key — order-independent canonicalization.
        assert_eq!(
            key,
            cassette_file(dir, "https://api.typesafe.ai", "jev-1.13.0", &body)
        );
    }

    // --- Endpoint construction ---

    #[test]
    fn endpoint_appends_the_systemone_path_once() {
        assert_eq!(
            endpoint("https://api.typesafe.ai"),
            "https://api.typesafe.ai/v1/systemone"
        );
        assert_eq!(
            endpoint("https://zen.example/gateway/"),
            "https://zen.example/gateway/v1/systemone"
        );
    }

    // --- Retry classification ---

    #[test]
    fn transient_statuses_are_retried_and_configuration_statuses_are_not() {
        // Rate limits and server-side failures (500-599 includes the
        // provider's own 529) can succeed on a second try.
        for status in [429, 500, 502, 503, 529, 599] {
            assert!(is_retryable(status), "HTTP {} should be retried", status);
        }
        // Everything else repeats identically, so retrying only delays the
        // loud message that tells the operator what to fix.
        for status in [400, 401, 403, 404, 422] {
            assert!(
                !is_retryable(status),
                "HTTP {} must abort the first attempt",
                status
            );
        }
    }

    #[test]
    fn a_non_retryable_status_names_the_actual_remedy() {
        let auth = non_retryable_reason(401, "MY_TYPESAFE_KEY");
        assert!(auth.contains("MY_TYPESAFE_KEY"), "got: {auth}");
        assert!(auth.contains("authentication"), "got: {auth}");
        assert!(
            non_retryable_reason(403, "MY_TYPESAFE_KEY").contains("MY_TYPESAFE_KEY"),
            "403 must name the env var too"
        );

        let manifest = non_retryable_reason(422, "MY_TYPESAFE_KEY");
        assert!(manifest.contains("fix the manifest"), "got: {manifest}");

        let other = non_retryable_reason(404, "MY_TYPESAFE_KEY");
        assert!(other.contains("not transient"), "got: {other}");
    }
}
