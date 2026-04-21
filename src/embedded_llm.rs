use anyhow::{Context, Result};
use std::path::{PathBuf};
use std::fs;
use std::num::NonZero;
use tokio::io::AsyncWriteExt;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::LlamaModel;
use llama_cpp_2::model::AddBos;

// Default to a small, fast model suitable for local error log analysis.
const DEFAULT_MODEL_URL: &str = "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf";

async fn ensure_model_downloaded(model_path: Option<&String>) -> Result<PathBuf> {
    let models_dir = PathBuf::from(".neuroplasticity/models");
    
    // Determine target path
    let target_path = if let Some(path_str) = model_path {
        let path = PathBuf::from(path_str);
        if path.exists() {
            return Ok(path);
        }
        path
    } else {
        if !models_dir.exists() {
            fs::create_dir_all(&models_dir).context("Failed to create .neuroplasticity/models directory")?;
        }
        models_dir.join("qwen2.5-coder-7b-instruct-q4_k_m.gguf")
    };

    if target_path.exists() {
        return Ok(target_path);
    }

    println!("Local model not found at {:?}. Downloading default 4-bit model...", target_path);
    println!("This will take a few minutes but only happens once.");

    let response = reqwest::get(DEFAULT_MODEL_URL).await.context("Failed to download model")?;
    
    if !response.status().is_success() {
        anyhow::bail!("Failed to download model: HTTP {}", response.status());
    }

    let mut file = tokio::fs::File::create(&target_path).await.context("Failed to create model file")?;
    let mut stream = response.bytes_stream();
    
    use futures_util::StreamExt;
    
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Error reading streaming model download")?;
        file.write_all(&chunk).await.context("Failed to write to model file")?;
    }
    
    println!("Model downloaded successfully.");

    Ok(target_path)
}

pub async fn run_embedded_llm(
    task_prompt: &str,
    failing_logs: &str,
    model_path: Option<&String>,
) -> Result<String> {
    let actual_model_path = ensure_model_downloaded(model_path).await?;
    
    println!("Initializing local llama.cpp engine with {:?}", actual_model_path);
    
    let backend = LlamaBackend::init().context("Failed to initialize llama backend")?;
    let model_params = LlamaModelParams::default();
    
    let model = LlamaModel::load_from_file(&backend, &actual_model_path, &model_params)
        .context("Failed to load model from file")?;

    let mut ctx_params = LlamaContextParams::default();
    ctx_params = ctx_params.with_n_ctx(Some(NonZero::new(8192).unwrap())); // Provide a decent context window for logs
    
    let mut ctx = model.new_context(&backend, ctx_params)
        .context("Failed to create llama context")?;

    let system_prompt = "You are the NeuroPlasticity Meta-Optimizer. Read the failing test logs and output ONLY a JSON rule to fix the agent's behavior. Format as a string: 'Rule: ...'";
    let user_prompt = format!("Task: {}\n\nFailing Logs:\n{}", task_prompt, failing_logs);
    
    // Quick prompt template (ChatML / Qwen format)
    let full_prompt = format!("<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n", system_prompt, user_prompt);
    
    let tokens = model.str_to_token(&full_prompt, AddBos::Always)
        .context("Failed to tokenize prompt")?;
        
    let mut batch = LlamaBatch::new(8192, 1);
    
    let last_index = tokens.len() - 1;
    for (i, &token) in tokens.iter().enumerate() {
        let is_last = i == last_index;
        batch.add(token, i as i32, &[0], is_last)?;
    }
    
    ctx.decode(&mut batch).context("llama.cpp decode failed")?;
    
    let mut generated_text = String::new();
    let mut n_cur = batch.n_tokens();
    let n_len = 1024; // max generated tokens
    
    while n_cur < batch.n_tokens() + n_len {
        let candidates_iter = ctx.candidates();
        let mut best_id: Option<llama_cpp_2::token::LlamaToken> = None;
        let mut best_logit = f32::NEG_INFINITY;
        
        for candidate in candidates_iter {
            if candidate.logit() > best_logit {
                best_logit = candidate.logit();
                best_id = Some(candidate.id());
            }
        }
        
        let new_token_id = best_id.unwrap_or_else(|| model.token_eos());
        
        if new_token_id == model.token_eos() {
            break;
        }
        
        // Use token_to_piece_bytes correctly
        // pub fn token_to_piece_bytes(
        //    &self,
        //    token: LlamaToken,
        //    max_len: usize,
        //    special: bool,
        //    lstrip: Option<NonZero<u16>>
        // ) -> Result<Vec<u8>, TokenToStringError>
        
        if let Ok(piece) = model.token_to_piece_bytes(new_token_id, 32, false, None) {
            let token_str = String::from_utf8_lossy(&piece).to_string();
            generated_text.push_str(&token_str);
        }
        
        batch.clear();
        batch.add(new_token_id, n_cur, &[0], true)?;
        ctx.decode(&mut batch)?;
        n_cur += 1;
    }
    
    Ok(generated_text.trim().to_string())
}
