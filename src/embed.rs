// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 John Anthony Mariquit <john@mariquit.com>

//! Candle `all-MiniLM-L6-v2` sentence embedder behind the unchanged `Embedder`
//! trait (outcome O2). Pure-Rust, CPU, offline: a BERT forward pass →
//! mean-pool over the attention mask → L2-normalize → 384-dim `Vec<f32>`.
//!
//! Zero network egress: the model directory is resolved from local paths only
//! and every one of the three weight files must already be present. A missing
//! file yields `EmbedError::ModelNotFound` — the caller maps that to the
//! `model_not_found` error envelope and a non-zero exit. No fetch code is
//! reachable from this module (the only network-touching command is
//! `lattice install`, in `install.rs`).

use std::path::{Path, PathBuf};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use tokenizers::Tokenizer;

use crate::vectors::Embedder;

/// Model identifier / cache subdirectory name.
pub const MODEL_NAME: &str = "all-MiniLM-L6-v2";

/// The three files a complete model directory must contain.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

/// Truncate file-level documents to this many tokens before the forward pass
/// (MVP-consistent with the file-level granularity of the store).
const MAX_TOKENS: usize = 256;

/// Errors distinguishable by the CLI: a missing/incomplete model maps to the
/// `model_not_found` envelope; anything else is an internal failure.
#[derive(Debug)]
pub enum EmbedError {
    /// The resolved model directory is absent or missing one of the three
    /// required files. Maps to the `model_not_found` error envelope.
    ModelNotFound(String),
    /// Model present but loading/inference failed (corrupt weights, tokenizer
    /// parse error, tensor-shape mismatch, ...). Maps to `internal`.
    Internal(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbedError::ModelNotFound(m) => write!(f, "{m}"),
            EmbedError::Internal(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Resolve the model directory (zero-egress, never fetches):
///   1. `$LATTICE_MODEL_DIR` (points directly at a model directory), else
///   2. `$HOME/.lattice/models/all-MiniLM-L6-v2/`.
pub fn resolve_model_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("LATTICE_MODEL_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".lattice")
        .join("models")
        .join(MODEL_NAME)
}

/// True iff the directory holds all three required model files.
pub fn model_present(dir: &Path) -> bool {
    MODEL_FILES.iter().all(|f| dir.join(f).is_file())
}

/// The `model_not_found` guard shared by `init` and `query`: verify the model
/// weights exist locally, without touching the network. Returns the resolved
/// directory on success.
pub fn ensure_model_present() -> std::result::Result<PathBuf, EmbedError> {
    let dir = resolve_model_dir();
    if model_present(&dir) {
        Ok(dir)
    } else {
        Err(EmbedError::ModelNotFound(format!(
            "candle model missing; run `lattice install` first (looked in {})",
            dir.display()
        )))
    }
}

/// Count WordPiece tokens in `text` with the already-installed MiniLM
/// tokenizer. Does not load model weights. Adds the tokenizer's special
/// tokens so the count is the one a forward pass would see.
pub fn count_tokens(text: &str) -> std::result::Result<usize, EmbedError> {
    let dir = ensure_model_present()?;
    let mut tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| EmbedError::Internal(format!("load tokenizer.json: {e}")))?;
    // The model truncates to 128 for the forward pass. Bench counts the corpus
    // an agent would actually read, so truncation is off here.
    tokenizer.with_truncation(None).ok();
    let encoding = tokenizer
        .encode(text, true)
        .map_err(|e| EmbedError::Internal(format!("tokenize: {e}")))?;
    Ok(encoding.get_ids().len())
}

/// Loaded candle BERT embedder. Constructed once per process; `embed` is pure
/// and deterministic for fixed weights (test-stable ranking).
pub struct CandleEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl CandleEmbedder {
    /// Load the model from a resolved directory. All three files must already
    /// be present (checked by `ensure_model_present` upstream); a missing file
    /// here still returns `ModelNotFound` rather than fetching anything.
    pub fn load(dir: &Path) -> std::result::Result<Self, EmbedError> {
        if !model_present(dir) {
            return Err(EmbedError::ModelNotFound(format!(
                "candle model missing; run `lattice install` first (looked in {})",
                dir.display()
            )));
        }
        let device = Device::Cpu;

        let config_bytes = std::fs::read(dir.join("config.json"))
            .map_err(|e| EmbedError::Internal(format!("read config.json: {e}")))?;
        let config: Config = serde_json::from_slice(&config_bytes)
            .map_err(|e| EmbedError::Internal(format!("parse config.json: {e}")))?;

        let tokenizer = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| EmbedError::Internal(format!("load tokenizer.json: {e}")))?;

        // SAFETY: mmap of a local file we control; the file is not mutated for
        // the lifetime of the VarBuilder / model.
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(
                &[dir.join("model.safetensors")],
                DTYPE,
                &device,
            )
            .map_err(|e| EmbedError::Internal(format!("load model.safetensors: {e}")))?
        };
        let model = BertModel::load(vb, &config)
            .map_err(|e| EmbedError::Internal(format!("build BERT model: {e}")))?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Convenience constructor: resolve the model dir and load. Surfaces
    /// `ModelNotFound` when the weights are absent.
    pub fn from_env() -> std::result::Result<Self, EmbedError> {
        let dir = ensure_model_present()?;
        Self::load(&dir)
    }

    /// Fallible embed used internally so load-time / inference failures can be
    /// surfaced as structured errors. The trait method wraps this.
    fn embed_checked(&self, text: &str) -> std::result::Result<Vec<f32>, EmbedError> {
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| EmbedError::Internal(format!("tokenize: {e}")))?;

        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        let mut mask: Vec<u32> = encoding.get_attention_mask().to_vec();
        if ids.len() > MAX_TOKENS {
            ids.truncate(MAX_TOKENS);
            mask.truncate(MAX_TOKENS);
        }
        let seq_len = ids.len();

        let input_ids = Tensor::new(ids.as_slice(), &self.device)
            .and_then(|t| t.reshape((1, seq_len)))
            .map_err(|e| EmbedError::Internal(format!("input_ids tensor: {e}")))?;
        let attn_u32 = Tensor::new(mask.as_slice(), &self.device)
            .and_then(|t| t.reshape((1, seq_len)))
            .map_err(|e| EmbedError::Internal(format!("attention tensor: {e}")))?;
        let token_type_ids = input_ids
            .zeros_like()
            .map_err(|e| EmbedError::Internal(format!("token_type_ids tensor: {e}")))?;

        // last_hidden_state: [1, seq_len, hidden(384)]
        let hidden = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attn_u32))
            .map_err(|e| EmbedError::Internal(format!("forward pass: {e}")))?;

        // Mean-pool over the attention mask, then L2-normalize.
        let attn_f = attn_u32
            .to_dtype(DType::F32)
            .map_err(|e| EmbedError::Internal(format!("mask cast: {e}")))?;
        let pooled = mean_pool(&hidden, &attn_f)
            .map_err(|e| EmbedError::Internal(format!("mean pool: {e}")))?;
        let normalized = l2_normalize(&pooled)
            .map_err(|e| EmbedError::Internal(format!("l2 normalize: {e}")))?;

        let vec: Vec<f32> = normalized
            .to_vec1()
            .map_err(|e| EmbedError::Internal(format!("to_vec1: {e}")))?;
        Ok(vec)
    }
}

/// Mean-pool `hidden` ([1, seq, hidden]) over the attention `mask`
/// ([1, seq]) → [hidden]. Masked (padding) positions are excluded; the sum is
/// divided by the count of attended tokens (clamped ≥ 1).
fn mean_pool(hidden: &Tensor, mask: &Tensor) -> candle_core::Result<Tensor> {
    // mask: [1, seq] -> [1, seq, 1] broadcast over hidden dim.
    let mask3 = mask.unsqueeze(2)?; // [1, seq, 1]
    let masked = hidden.broadcast_mul(&mask3)?; // [1, seq, hidden]
    let summed = masked.sum(1)?; // [1, hidden]
    let counts = mask.sum(1)?.clamp(1f32, f32::INFINITY)?; // [1]
    let counts = counts.unsqueeze(1)?; // [1, 1]
    let mean = summed.broadcast_div(&counts)?; // [1, hidden]
    mean.squeeze(0) // [hidden]
}

/// L2-normalize a 1-D tensor. A zero vector is returned unchanged (norm clamped
/// away from zero to avoid division by zero).
fn l2_normalize(v: &Tensor) -> candle_core::Result<Tensor> {
    let norm = v.sqr()?.sum_all()?.sqrt()?; // scalar
    let norm = norm.to_scalar::<f32>()?;
    if norm <= f32::EPSILON {
        return Ok(v.clone());
    }
    v.affine(1.0 / norm as f64, 0.0)
}

impl Embedder for CandleEmbedder {
    const DIM: usize = 384;

    fn embed(&self, text: &str) -> Vec<f32> {
        // The trait signature is infallible (O2, unchanged). Load-time failures
        // are surfaced before this is reached (CandleEmbedder::from_env /
        // ensure_model_present); an inference failure here is unexpected and
        // degrades to a zero vector rather than panicking. In practice the
        // guarded construction path means this is never hit with bad weights.
        self.embed_checked(text).unwrap_or_else(|_| vec![0.0f32; Self::DIM])
    }
}
