use crate::manifest::MetaLlmConfig;
use anyhow::{Context, Result};
use std::time::Duration;

/// Per-attempt HTTP deadline.
///
/// async `reqwest::Client` has NO default timeout (only the blocking client
/// does), and nothing else in the orchestrator wraps `ask_llm`, so without this
/// a wedged endpoint would hang the epoch forever.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// Transient failures (429 / 5xx / connect errors / malformed bodies) retry
/// with exponential backoff. Cloud endpoints rate-limit at 10 concurrent
/// requests, so 429s are expected under load.
const MAX_ATTEMPTS: u32 = 3;

/// Deterministic grading. A grader that flaps between runs poisons the
/// failure-fingerprint cache with verdicts that cannot be reproduced.
const DEFAULT_TEMPERATURE: f64 = 0.0;

/// Generous enough for a reasoning model, bounded so one evaluator can't eat
/// the whole epoch.
const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Token accounting for one completion, as reported by the provider (F7).
///
/// Most OpenAI-compatible endpoints return this in `usage`; local inference
/// does not, in which case cost is simply zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }

    /// Cost in USD, given per-1k-token prices. Prices are configuration rather
    /// than a built-in table: they change constantly, and guessing wrong would
    /// make a budget cap quietly meaningless.
    pub fn cost_usd(&self, input_per_1k: f64, output_per_1k: f64) -> f64 {
        (self.prompt_tokens as f64 / 1000.0) * input_per_1k
            + (self.completion_tokens as f64 / 1000.0) * output_per_1k
    }
}

/// Parse `usage` out of a response body, if the provider reported one.
fn parse_usage(body: &str) -> Option<Usage> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let usage = value.get("usage")?;
    Some(Usage {
        prompt_tokens: usage
            .get("prompt_tokens")
            .or_else(|| usage.get("input_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        completion_tokens: usage
            .get("completion_tokens")
            .or_else(|| usage.get("output_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// A completion request against the configured meta-llm.
pub struct CompletionSpec<'a> {
    pub system_prompt: &'a str,
    pub user_prompt: &'a str,
    /// JSON Schema to constrain the reply to. When present the reply is parsed
    /// as JSON and retried if malformed. The prompt must describe the shape
    /// too, so the contract still holds if the provider strips
    /// `response_format`.
    pub json_schema: Option<serde_json::Value>,
}

/// Free-text completion; returns the raw assistant text.
pub async fn ask_llm(
    config: &MetaLlmConfig,
    system_prompt: &str,
    user_prompt: &str,
) -> Result<String> {
    let spec = CompletionSpec {
        system_prompt,
        user_prompt,
        json_schema: None,
    };
    match complete(config, &spec).await? {
        serde_json::Value::String(text) => Ok(text),
        other => Ok(other.to_string()),
    }
}

/// Structured completion. A reply that isn't valid JSON is retried rather than
/// returned, so a garbled verdict is never mistaken for a real one.
pub async fn ask_llm_json(
    config: &MetaLlmConfig,
    spec: &CompletionSpec<'_>,
) -> Result<serde_json::Value> {
    complete(config, spec).await
}

async fn complete(config: &MetaLlmConfig, spec: &CompletionSpec<'_>) -> Result<serde_json::Value> {
    complete_tracked(config, spec).await.map(|(value, _)| value)
}

/// As [`complete`], but also reports token usage when the provider does (F7).
pub async fn complete_tracked(
    config: &MetaLlmConfig,
    spec: &CompletionSpec<'_>,
) -> Result<(serde_json::Value, Option<Usage>)> {
    let expect_json = spec.json_schema.is_some();

    if config.provider == "embedded" {
        #[cfg(feature = "embedded-llm")]
        {
            let text = crate::embedded_llm::run_embedded_llm(
                spec.system_prompt,
                spec.user_prompt,
                config.model_path.as_ref(),
            )
            .await?;
            // Local inference is not billed, so there is nothing to accrue.
            let value = if expect_json {
                parse_json_object(&text)?
            } else {
                serde_json::Value::String(text)
            };
            return Ok((value, None));
        }
        #[cfg(not(feature = "embedded-llm"))]
        {
            anyhow::bail!(
                "The 'embedded' provider requires the 'embedded-llm' feature to be enabled during build."
            );
        }
    }

    let (url, token) = resolve_endpoint(config).await?;
    let style = resolve_style(config, &url)?;
    let payload = build_payload(config, spec, style);
    post_completion(&url, token.as_deref(), payload, style, expect_json).await
}

/// Report cumulative spend to the budget, using configured token prices.
pub fn report_spend(
    tracker: &mut crate::manifest::BudgetTracker,
    usage: &Usage,
    budget: &crate::manifest::Budget,
) {
    let input_price = budget.cost_per_1k_input_usd.unwrap_or(0.0);
    let output_price = budget.cost_per_1k_output_usd.unwrap_or(0.0);
    let cost = usage.cost_usd(input_price, output_price);
    if cost > 0.0 {
        println!(
            "   💰 {:.4} USD ({} prompt + {} completion tokens)",
            cost, usage.prompt_tokens, usage.completion_tokens
        );
    }
    tracker.add_spend(cost);
}

/// Which OpenAI wire protocol to speak.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ApiStyle {
    ChatCompletions,
    Responses,
}

/// Explicit `api_style` wins; otherwise a `base_url` that already points at a
/// `/responses` endpoint implies it.
fn resolve_style(config: &MetaLlmConfig, url: &str) -> Result<ApiStyle> {
    match config.api_style.as_deref() {
        Some("chat_completions") => Ok(ApiStyle::ChatCompletions),
        Some("responses") => Ok(ApiStyle::Responses),
        Some(other) => anyhow::bail!(
            "Unsupported api_style '{}' (expected 'chat_completions' or 'responses')",
            other
        ),
        None => Ok(if url.trim_end_matches('/').ends_with("/responses") {
            ApiStyle::Responses
        } else {
            ApiStyle::ChatCompletions
        }),
    }
}

/// Resolve the endpoint URL and bearer token for the configured provider.
async fn resolve_endpoint(config: &MetaLlmConfig) -> Result<(String, Option<String>)> {
    if config.provider == "github" {
        anyhow::bail!(
            "Provider 'github' is no longer available: GitHub Models was retired on 2026-07-30 \
             (https://github.blog/changelog/2026-07-30-github-models-is-now-retired/). \
             Use provider 'embedded' for offline llama.cpp inference, or 'custom' with \
             base_url/api_key_env for any OpenAI-compatible endpoint."
        );
    }

    // 2. Generic OpenAI-compatible endpoint
    let url = config
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1/chat/completions".to_string());
    let env_var = config.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");

    let token = resolve_api_key(env_var, &url)?;

    Ok((url, token))
}

/// Resolve the bearer token for an `api_key_env` name, enforcing the
/// credential-name security guard.
///
/// Shared with the TypeSafe client (`crate::typesafe`) so *every* provider
/// that reads a manifest-named environment variable validates it identically:
/// a malicious manifest must not be able to point any provider at
/// `AWS_SECRET_ACCESS_KEY` or `SSH_PRIVATE_KEY` and have the orchestrator
/// echo it to a remote endpoint.
///
/// Returns `Ok(None)` only for loopback endpoints (llama-server, Ollama, …)
/// with an empty key — nothing leaves the machine there, so no credential is
/// required. Anywhere else an empty key is a hard error: running a grader
/// without its credential would surface later as confusing 401s.
pub fn resolve_api_key(env_var: &str, url: &str) -> Result<Option<String>> {
    // P0 Security Fix: Prevent exfiltration of arbitrary host env vars (like AWS_SECRET_ACCESS_KEY or SSH_PRIVATE_KEY) via malicious plasticity.json
    // Hardened: Must exactly match known patterns, not just suffixes
    let is_valid_env_var = env_var == "API_KEY"
        || env_var == "GITHUB_TOKEN"
        || env_var == "OPENAI_API_KEY"
        || env_var == "ANTHROPIC_API_KEY"
        || env_var == "GEMINI_API_KEY"
        || env_var == "GROQ_API_KEY"
        || env_var == "XAI_API_KEY"
        || env_var == "DEEPSEEK_API_KEY"
        || env_var == "TOGETHER_API_KEY";

    if !is_valid_env_var {
        static API_KEY_REGEX: once_cell::sync::Lazy<regex::Regex> =
            once_cell::sync::Lazy::new(|| {
                regex::Regex::new(r"^[A-Z][A-Z0-9_]*_(API_KEY|TOKEN)$").unwrap()
            });
        let is_safe_pattern = API_KEY_REGEX.is_match(env_var)
            && !env_var.contains("SECRET")
            && !env_var.contains("PRIVATE")
            && !env_var.contains("AWS")
            && !env_var.contains("SSH")
            && !env_var.contains("GCP")
            && !env_var.contains("AZURE");

        if !is_safe_pattern {
            anyhow::bail!(
                "Security Exception: To prevent credential exfiltration, `api_key_env` must be a standard LLM API key name. Attempted to use: {}",
                env_var
            );
        }
    }

    let api_key = std::env::var(env_var).unwrap_or_default();

    // Loopback servers (llama-server, Ollama, LM Studio, vLLM) need no credential.
    // The env var *name* is still validated above, so this does not widen the
    // exfiltration guard: nothing is sent off-box either way.
    if api_key.is_empty() && !is_loopback(url) {
        anyhow::bail!("API key environment variable {} is empty.", env_var);
    }

    let token = if api_key.is_empty() {
        None
    } else {
        Some(api_key)
    };

    Ok(token)
}

/// True when the URL targets the local machine only.
fn is_loopback(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        // `host_str()` brackets IPv6 literals (`[::1]`), so strip brackets
        // before comparing.
        Some(host) => matches!(
            host.trim_start_matches('[').trim_end_matches(']'),
            "localhost" | "127.0.0.1" | "::1"
        ),
        None => false,
    }
}

/// Wrap a schema in a provider `response_format` object.
fn json_schema_format(schema: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "neuroplasticity",
            "strict": true,
            "schema": schema
        }
    })
}

/// Build the request payload for the configured protocol.
///
/// `temperature` defaults to 0 so graders are reproducible. Providers that
/// reject these fields (e.g. reasoning models that only allow the default
/// temperature, or ones without structured-output support) are detected from
/// the HTTP 400 body and the field is dropped on retry — see
/// [`strip_unsupported_field`].
fn build_payload(
    config: &MetaLlmConfig,
    spec: &CompletionSpec<'_>,
    style: ApiStyle,
) -> serde_json::Value {
    let temperature = config.temperature.unwrap_or(DEFAULT_TEMPERATURE);
    let max_tokens = config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);

    let (mut payload, schema_key) = match style {
        ApiStyle::ChatCompletions => (
            serde_json::json!({
                "model": config.model,
                "temperature": temperature,
                "max_tokens": max_tokens,
                "messages": [
                    { "role": "system", "content": spec.system_prompt },
                    { "role": "user", "content": spec.user_prompt }
                ]
            }),
            "response_format",
        ),
        ApiStyle::Responses => (
            serde_json::json!({
                "model": config.model,
                "temperature": temperature,
                "max_output_tokens": max_tokens,
                "input": [
                    {
                        "role": "system",
                        "content": [{ "type": "input_text", "text": spec.system_prompt }]
                    },
                    {
                        "role": "user",
                        "content": [{ "type": "input_text", "text": spec.user_prompt }]
                    }
                ]
            }),
            "text",
        ),
    };

    if let Some(schema) = &spec.json_schema {
        let format = json_schema_format(schema);
        if style == ApiStyle::Responses {
            // Responses nests the output contract under `text.format`.
            payload
                .as_object_mut()
                .unwrap()
                .insert("text".to_string(), serde_json::json!({ "format": format }));
        } else {
            payload
                .as_object_mut()
                .unwrap()
                .insert(schema_key.to_string(), format);
        }
    }

    payload
}

/// Optional request fields we know how to drop when a provider rejects them,
/// matched against the provider's error text.
fn optional_fields(style: ApiStyle) -> &'static [&'static str] {
    match style {
        ApiStyle::ChatCompletions => &["temperature", "max_tokens", "response_format"],
        // In the Responses API the output contract lives under `text`.
        ApiStyle::Responses => &["temperature", "max_output_tokens", "json_schema"],
    }
}

/// If the provider rejected one of our optional fields by name, remove it from
/// the payload and report it so the caller can retry. Returns `None` when the
/// error is about something else (bad model name, auth, context length...).
fn strip_unsupported_field(
    payload: &mut serde_json::Value,
    error_body: &str,
    style: ApiStyle,
) -> Option<&'static str> {
    let lowered = error_body.to_lowercase();
    let field = optional_fields(style)
        .iter()
        .find(|f| lowered.contains(**f))
        .copied()?;

    let key = match (style, field) {
        (ApiStyle::Responses, "json_schema") => "text",
        (_, other) => other,
    };
    payload.as_object_mut()?.remove(key);
    Some(field)
}

/// POST a completion, surfacing API errors instead of swallowing them and
/// retrying malformed or off-contract replies.
async fn post_completion(
    url: &str,
    token: Option<&str>,
    mut payload: serde_json::Value,
    style: ApiStyle,
    expect_json: bool,
) -> Result<(serde_json::Value, Option<Usage>)> {
    let client = reqwest::Client::builder()
        .timeout(ATTEMPT_TIMEOUT)
        .build()
        .context("Failed to build HTTP client")?;

    let mut last_error = anyhow::anyhow!("LLM request was not attempted");

    for attempt in 1..=MAX_ATTEMPTS {
        if attempt > 1 {
            let backoff_ms = 500u64.saturating_mul(1 << (attempt - 2));
            eprintln!(
                "⏳ Retrying LLM request in {}ms (attempt {}/{})...",
                backoff_ms, attempt, MAX_ATTEMPTS
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

        let mut request = client.post(url).json(&payload);
        if let Some(token) = token.filter(|t| !t.is_empty()) {
            request = request.bearer_auth(token);
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                last_error = anyhow::Error::new(e)
                    .context(format!("Failed to send request to LLM API ({})", url));
                continue;
            }
        };

        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // Never let an API error masquerade as model output: the caller
            // would score it as a grader verdict or hand it to the optimizer.
            let retryable = status.as_u16() == 429 || status.is_server_error();

            last_error = anyhow::anyhow!(
                "LLM API request failed: HTTP {} from {} — {}",
                status,
                url,
                truncate(&body, 500)
            );

            if status.as_u16() == 400 {
                if let Some(field) = strip_unsupported_field(&mut payload, &body, style) {
                    eprintln!(
                        "ℹ️ Provider rejected optional field '{}'; retrying without it.",
                        field
                    );
                    continue;
                }
            }

            if !retryable {
                break;
            }
            continue;
        }

        let text = match extract_text(&body, style) {
            Ok(text) => text,
            Err(e) => {
                last_error = e;
                // Malformed/empty success bodies are usually transient.
                continue;
            }
        };

        let usage = parse_usage(&body);

        if !expect_json {
            return Ok((serde_json::Value::String(text), usage));
        }

        match parse_json_object(&text) {
            Ok(value) => return Ok((value, usage)),
            Err(e) => {
                // Off-contract completion (prose, markdown fence, truncated
                // JSON): retry rather than scoring garbage.
                last_error = e;
                continue;
            }
        }
    }

    Err(last_error.context(format!(
        "LLM request to {} failed after {} attempt(s)",
        url, MAX_ATTEMPTS
    )))
}

/// Pull the assistant text out of a response, for either protocol.
fn extract_text(body: &str, style: ApiStyle) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .with_context(|| format!("LLM response was not valid JSON: {}", truncate(body, 500)))?;

    if let Some(message) = value.pointer("/error/message").and_then(|m| m.as_str()) {
        anyhow::bail!("LLM API returned an error object: {}", message);
    }

    let text = match style {
        ApiStyle::ChatCompletions => match value.pointer("/choices/0/message/content") {
            Some(serde_json::Value::String(s)) => s.clone(),
            // A few OpenAI-compatible endpoints return an array of content parts.
            Some(serde_json::Value::Array(parts)) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            _ => anyhow::bail!(
                "LLM response is missing choices[0].message.content: {}",
                truncate(body, 500)
            ),
        },
        ApiStyle::Responses => extract_responses_text(&value, body)?,
    };

    let text = text.trim().to_string();
    if text.is_empty() {
        // `Ok("")` would be scored as a grader verdict (never PASS) or written
        // out as an empty optimization rule, so treat it as an error instead.
        anyhow::bail!("LLM returned an empty completion: {}", truncate(body, 500));
    }

    Ok(text)
}

/// Walk a Responses-API `output` array, skipping reasoning items.
fn extract_responses_text(value: &serde_json::Value, body: &str) -> Result<String> {
    if let Some(text) = value.get("output_text").and_then(|t| t.as_str()) {
        return Ok(text.to_string());
    }

    let output = value
        .get("output")
        .and_then(|o| o.as_array())
        .ok_or_else(|| {
            anyhow::anyhow!("LLM response is missing output[]: {}", truncate(body, 500))
        })?;

    let mut parts = Vec::new();
    for item in output {
        if item.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue; // reasoning / tool-call items carry no user-facing text
        }
        let Some(content) = item.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(|t| t.as_str()) == Some("output_text") {
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    parts.push(text.to_string());
                }
            }
        }
    }

    if parts.is_empty() {
        anyhow::bail!(
            "LLM response contained no output_text message item: {}",
            truncate(body, 500)
        );
    }
    Ok(parts.join(""))
}

/// Parse a JSON object out of a model reply, tolerating markdown fences and
/// chatty preambles.
pub fn parse_json_object(text: &str) -> Result<serde_json::Value> {
    let trimmed = text.trim();

    // ```json ... ``` or ``` ... ```
    let unfenced = if trimmed.starts_with("```") {
        let after_open = trimmed.trim_start_matches('`');
        let body = match after_open.split_once('\n') {
            Some((_, rest)) => rest,
            None => after_open,
        };
        body.trim_end().trim_end_matches('`').trim()
    } else {
        trimmed
    };

    let start = unfenced.find('{');
    let end = unfenced.rfind('}');
    match (start, end) {
        (Some(start), Some(end)) if end > start => serde_json::from_str(&unfenced[start..=end])
            .with_context(|| {
                format!(
                    "LLM reply claimed to be JSON but would not parse: {}",
                    truncate(&unfenced[start..=end], 300)
                )
            }),
        _ => anyhow::bail!(
            "LLM reply contained no JSON object: {}",
            truncate(unfenced, 300)
        ),
    }
}

/// Truncate on a character boundary so multibyte responses can't panic.
fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::optimizer;
    #[test]
    fn extracts_plain_string_content() {
        let body = r#"{"choices":[{"message":{"content":"  PASS: looks good  "}}]}"#;
        assert_eq!(
            extract_text(body, ApiStyle::ChatCompletions).unwrap(),
            "PASS: looks good"
        );
    }

    #[test]
    fn joins_array_content_parts() {
        let body = r#"{"choices":[{"message":{"content":[{"type":"text","text":"PASS"},{"type":"text","text":" fine"}]}}]}"#;
        assert_eq!(
            extract_text(body, ApiStyle::ChatCompletions).unwrap(),
            "PASS fine"
        );
    }

    /// Regression: an API error used to fall through to `Ok("Fallback error")`
    /// and get scored as a grader verdict.
    #[test]
    fn surfaces_error_objects_instead_of_faking_output() {
        let body = r#"{"error":{"message":"Incorrect API key provided"}}"#;
        let err = extract_text(body, ApiStyle::ChatCompletions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Incorrect API key provided"), "{}", err);
    }

    #[test]
    fn rejects_response_without_choices() {
        let body = r#"{"object":"error","message":"rate limit exceeded","type":"tokens"}"#;
        let err = extract_text(body, ApiStyle::ChatCompletions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing choices[0]"), "{}", err);
    }

    #[test]
    fn rejects_empty_completion() {
        let body = r#"{"choices":[{"message":{"content":"   "}}]}"#;
        let err = extract_text(body, ApiStyle::ChatCompletions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty completion"), "{}", err);
    }

    #[test]
    fn rejects_non_json_body() {
        let err = extract_text("<html>502 Bad Gateway</html>", ApiStyle::ChatCompletions)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not valid JSON"), "{}", err);
    }

    // --- Responses API ---

    #[test]
    fn extracts_responses_output_text_and_skips_reasoning() {
        let body = r#"{
            "id": "resp_1",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking..."}]},
                {"type": "message", "content": [{"type": "output_text", "text": "{\"verdict\":\"PASS\"}"}]}
            ]
        }"#;
        let text = extract_text(body, ApiStyle::Responses).unwrap();
        assert_eq!(text, "{\"verdict\":\"PASS\"}");
    }

    #[test]
    fn extracts_responses_flattened_output_text() {
        let body = r#"{"output_text":"{\"verdict\":\"FAIL\"}","output":[]}"#;
        assert_eq!(
            extract_text(body, ApiStyle::Responses).unwrap(),
            "{\"verdict\":\"FAIL\"}"
        );
    }

    #[test]
    fn rejects_responses_with_only_reasoning() {
        let body =
            r#"{"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"hmm"}]}]}"#;
        let err = extract_text(body, ApiStyle::Responses)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no output_text"), "{}", err);
    }

    #[test]
    fn infers_api_style_from_base_url() {
        let cfg = MetaLlmConfig {
            provider: "custom".to_string(),
            model: "gpt-5.5".to_string(),
            base_url: Some("https://opencode.ai/zen/v1/responses".to_string()),
            api_key_env: None,
            model_path: None,
            temperature: None,
            max_tokens: None,
            api_style: None,
        };
        assert_eq!(
            resolve_style(&cfg, cfg.base_url.as_deref().unwrap()).unwrap(),
            ApiStyle::Responses
        );

        let chat = MetaLlmConfig {
            base_url: Some("https://opencode.ai/zen/v1/chat/completions".to_string()),
            ..cfg
        };
        assert_eq!(
            resolve_style(&chat, chat.base_url.as_deref().unwrap()).unwrap(),
            ApiStyle::ChatCompletions
        );

        let bad = MetaLlmConfig {
            api_style: Some("grpc".to_string()),
            ..chat
        };
        assert!(resolve_style(&bad, "https://example.com").is_err());
    }

    #[test]
    fn builds_responses_payload_with_nested_text_format() {
        let cfg = MetaLlmConfig {
            provider: "custom".to_string(),
            model: "gpt-5.5".to_string(),
            base_url: None,
            api_key_env: None,
            model_path: None,
            temperature: None,
            max_tokens: None,
            api_style: Some("responses".to_string()),
        };
        let spec = CompletionSpec {
            system_prompt: "sys",
            user_prompt: "usr",
            json_schema: Some(serde_json::json!({"type": "object"})),
        };
        let payload = build_payload(&cfg, &spec, ApiStyle::Responses);

        assert_eq!(payload["model"], "gpt-5.5");
        assert_eq!(payload["max_output_tokens"], DEFAULT_MAX_TOKENS);
        assert_eq!(payload["input"][0]["content"][0]["text"], "sys");
        assert_eq!(
            payload["text"]["format"]["json_schema"]["schema"]["type"],
            "object"
        );
    }

    // --- Structured replies ---

    #[test]
    fn parses_json_reply_with_markdown_fence() {
        let reply = "```json\n{\"verdict\": \"PASS\", \"reason\": \"valid\"}\n```";
        let value = parse_json_object(reply).unwrap();
        assert_eq!(value["verdict"], "PASS");
    }

    #[test]
    fn parses_json_reply_with_preamble() {
        let reply =
            "Here is my verdict:\n{\"rule\": \"Do not wrap JSON in fences.\"}\nHope that helps!";
        let value = parse_json_object(reply).unwrap();
        assert_eq!(value["rule"], "Do not wrap JSON in fences.");
    }

    /// Regression: a prose-only reply must be rejected so it is retried rather
    /// than scored as a verdict.
    #[test]
    fn rejects_prose_only_reply() {
        let err = parse_json_object("PASS, the output looks fine to me.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no JSON object"), "{}", err);
    }

    #[test]
    fn detects_loopback_hosts_only() {
        assert!(is_loopback("http://localhost:11434/v1/chat/completions"));
        assert!(is_loopback("http://127.0.0.1:8080/v1/chat/completions"));
        assert!(is_loopback("http://[::1]:8080/v1/chat/completions"));

        // Host-suffix lookalikes must NOT count as local.
        assert!(!is_loopback("http://localhost.attacker.example/v1"));
        assert!(!is_loopback("https://api.openai.com/v1/chat/completions"));
        assert!(!is_loopback("not a url"));
    }

    #[test]
    fn strips_provider_rejected_fields() {
        let mut payload = serde_json::json!({"model": "m", "temperature": 0.0, "max_tokens": 1024});

        let stripped = strip_unsupported_field(
            &mut payload,
            "Unsupported parameter: 'temperature' is not supported with this model.",
            ApiStyle::ChatCompletions,
        );
        assert_eq!(stripped, Some("temperature"));
        assert!(payload.get("temperature").is_none());
        assert!(payload.get("max_tokens").is_some());

        let stripped = strip_unsupported_field(
            &mut payload,
            "model not found: totally-unknown",
            ApiStyle::ChatCompletions,
        );
        assert_eq!(stripped, None);
        assert!(payload.get("max_tokens").is_some());
    }

    /// Structured output support is not universal; a 400 naming
    /// `response_format` must degrade to prompt-only JSON, not fail the run.
    #[test]
    fn strips_rejected_response_format() {
        let mut payload = serde_json::json!({
            "model": "m",
            "response_format": {"type": "json_schema"},
            "temperature": 0.0
        });
        let stripped = strip_unsupported_field(
            &mut payload,
            "Unsupported parameter: 'response_format' is not supported.",
            ApiStyle::ChatCompletions,
        );
        assert_eq!(stripped, Some("response_format"));
        assert!(payload.get("response_format").is_none());
        assert!(payload.get("temperature").is_some());
    }

    /// In the Responses API the output contract lives under `text`.
    #[test]
    fn strips_rejected_responses_json_schema() {
        let mut payload = serde_json::json!({
            "model": "m",
            "text": {"format": {"type": "json_schema"}},
            "max_output_tokens": 1024
        });
        let stripped = strip_unsupported_field(
            &mut payload,
            "json_schema is not supported by this model",
            ApiStyle::Responses,
        );
        assert_eq!(stripped, Some("json_schema"));
        assert!(payload.get("text").is_none());
        assert!(payload.get("max_output_tokens").is_some());
    }

    /// A model echoing the old "Rule: " instruction must not leak that prefix
    /// into the persisted constraint.
    #[test]
    fn strips_rule_prefix_and_quoting_from_generated_rule() {
        assert_eq!(
            optimizer::validate_rule("Rule: \"Do not wrap JSON in markdown fences.\"").unwrap(),
            "Do not wrap JSON in markdown fences."
        );
        assert_eq!(
            optimizer::validate_rule("```\nDo not use first person.\n```").unwrap(),
            "Do not use first person."
        );
    }

    #[test]
    fn rejects_empty_or_oversized_generated_rules() {
        assert!(optimizer::validate_rule("   ").is_err());
        assert!(optimizer::validate_rule("Rule:").is_err());
        let huge = "a".repeat(500);
        assert!(optimizer::validate_rule(&huge).is_err());
    }

    /// Regression: the grader used to do `starts_with("PASS")`, so a reply like
    /// "**PASS** — valid JSON" scored as a failure.
    #[test]
    fn verdict_is_read_from_the_json_field_not_by_prefix_matching() {
        let reply = "```json\n{\"verdict\": \"PASS\", \"reason\": \"output is valid JSON\"}\n```";
        let value = parse_json_object(reply).unwrap();
        let verdict = value["verdict"].as_str().unwrap().trim().to_uppercase();
        assert_eq!(verdict, "PASS");
    }

    #[test]
    fn truncates_multibyte_text_without_panicking() {
        let text = "日本語テキスト";
        assert_eq!(truncate(text, 2).chars().count(), 3); // 2 chars + ellipsis
        assert_eq!(truncate("short", 10), "short");
    }

    /// Round-trip through a real OpenAI-compatible endpoint.
    /// Configure with: NP_TEST_BASE_URL, NP_TEST_API_KEY (optional on loopback), NP_TEST_MODEL
    /// Run with: cargo test --features embedded-llm -- --ignored
    #[tokio::test]
    #[ignore = "requires a reachable OpenAI-compatible endpoint via NP_TEST_BASE_URL"]
    async fn custom_endpoint_round_trip() {
        let base_url = match std::env::var("NP_TEST_BASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!(
                    "skipping: set NP_TEST_BASE_URL (and optionally NP_TEST_API_KEY / NP_TEST_MODEL)"
                );
                return;
            }
        };
        let config = MetaLlmConfig {
            provider: "custom".to_string(),
            model: std::env::var("NP_TEST_MODEL").unwrap_or_else(|_| "test-model".to_string()),
            base_url: Some(base_url),
            api_key_env: std::env::var("NP_TEST_API_KEY_ENV").ok(),
            model_path: None,
            temperature: Some(0.0),
            max_tokens: Some(32),
            api_style: std::env::var("NP_TEST_API_STYLE").ok(),
        };
        // Exercises the structured path: a schema is requested, the reply must
        // parse as JSON, and the value is read out by key.
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "reply": { "type": "string" } },
            "required": ["reply"],
            "additionalProperties": false
        });
        let spec = CompletionSpec {
            system_prompt: "You are an echo bot. Reply with JSON only.",
            user_prompt: "Return {\"reply\": \"PONG\"} and nothing else.",
            json_schema: Some(schema),
        };
        let value = ask_llm_json(&config, &spec)
            .await
            .expect("LLM round trip failed");
        println!("Provider replied: {}", value);
        let reply = value
            .get("reply")
            .and_then(|r| r.as_str())
            .unwrap_or_default();
        assert!(
            reply.to_uppercase().contains("PONG"),
            "unexpected reply: {}",
            value
        );
    }
}

#[cfg(test)]
mod usage_tests {
    use super::{Usage, parse_usage};

    #[test]
    fn reads_openai_style_usage() {
        let body = r#"{"choices":[{"message":{"content":"hi"}}],
                      "usage":{"prompt_tokens":120,"completion_tokens":45}}"#;
        let u = parse_usage(body).unwrap();
        assert_eq!(u.prompt_tokens, 120);
        assert_eq!(u.completion_tokens, 45);
        assert_eq!(u.total_tokens(), 165);
    }

    /// Some providers use the Responses API's field names.
    #[test]
    fn reads_responses_style_usage() {
        let body = r#"{"output":[],"usage":{"input_tokens":10,"output_tokens":3}}"#;
        let u = parse_usage(body).unwrap();
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 3);
    }

    #[test]
    fn absent_usage_is_none_not_zero() {
        assert!(parse_usage(r#"{"choices":[]}"#).is_none());
    }

    #[test]
    fn cost_is_computed_from_configured_prices() {
        let u = Usage {
            prompt_tokens: 1_000,
            completion_tokens: 500,
        };
        // 1k prompt @ 0.0001 + 0.5k completion @ 0.0002
        assert!((u.cost_usd(0.0001, 0.0002) - 0.0002).abs() < 1e-12);
    }

    #[test]
    fn a_free_call_costs_nothing() {
        let u = Usage::default();
        assert_eq!(u.cost_usd(0.001, 0.002), 0.0);
    }
}
