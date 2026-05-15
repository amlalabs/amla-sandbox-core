//! `Model2Vec` text embedder for semantic tool search.
//!
//! This module provides a WASM-compatible text embedding implementation using
//! the `Model2Vec` approach: static token embeddings with mean pooling.
//!
//! The model (`potion-base-2M`) is bundled at compile time (~8MB total):
//! - 64-dimensional embeddings
//! - ~32K vocabulary
//! - MIT licensed
//!
//! ## How it works
//!
//! 1. Tokenize input text using `HuggingFace` tokenizers
//! 2. Look up each token's pre-computed embedding
//! 3. Mean-pool the embeddings
//! 4. L2-normalize the result
//!
//! This is 500x faster than transformer models and fully WASM-compatible.

use std::sync::OnceLock;

use safetensors::SafeTensors;
use tokenizers::Tokenizer;

/// Bundled model files (compiled into the binary).
const MODEL_BYTES: &[u8] = include_bytes!("../models/potion-base-2M/model.safetensors");
const TOKENIZER_JSON: &str = include_str!("../models/potion-base-2M/tokenizer.json");

/// Embedding dimension for potion-base-2M.
pub const EMBEDDING_DIM: usize = 64;

/// Global embedder instance (lazy-initialized).
static EMBEDDER: OnceLock<Result<Embedder, EmbedError>> = OnceLock::new();

/// Error type for embedding operations.
#[derive(Debug, Clone)]
pub struct EmbedError(pub String);

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Embedding error: {}", self.0)
    }
}

impl std::error::Error for EmbedError {}

/// `Model2Vec` text embedder.
///
/// Uses static token embeddings with mean pooling for fast, lightweight
/// text embedding generation.
pub struct Embedder {
    tokenizer: Tokenizer,
    embeddings: Vec<f32>, // Flattened [vocab_size, EMBEDDING_DIM]
    vocab_size: usize,
}

impl Embedder {
    /// Get or create the global embedder instance.
    ///
    /// The embedder is lazily initialized on first use.
    pub fn global() -> Result<&'static Embedder, EmbedError> {
        EMBEDDER
            .get_or_init(Self::new)
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Create a new embedder from bundled model files.
    fn new() -> Result<Self, EmbedError> {
        // Load tokenizer
        let tokenizer = Tokenizer::from_bytes(TOKENIZER_JSON.as_bytes())
            .map_err(|e| EmbedError(format!("Failed to load tokenizer: {e}")))?;

        // Load embeddings from safetensors
        let tensors = SafeTensors::deserialize(MODEL_BYTES)
            .map_err(|e| EmbedError(format!("Failed to load model: {e}")))?;

        // Get the embedding tensor (should be named "embeddings" or similar)
        let tensor = tensors
            .tensor("embeddings")
            .or_else(|_| tensors.tensor("weight"))
            .or_else(|_| tensors.tensor("embedding"))
            .map_err(|_| EmbedError("No embedding tensor found in model".to_string()))?;

        let shape = tensor.shape();
        if shape.len() != 2 {
            return Err(EmbedError(format!(
                "Expected 2D embedding tensor, got shape: {shape:?}"
            )));
        }

        let vocab_size = shape[0];
        let dim = shape[1];
        if dim != EMBEDDING_DIM {
            return Err(EmbedError(format!(
                "Expected {EMBEDDING_DIM}-dim embeddings, got {dim}"
            )));
        }

        // Convert bytes to f32 (assuming F32 dtype)
        let data = tensor.data();
        if data.len() != vocab_size * dim * 4 {
            return Err(EmbedError(format!(
                "Unexpected tensor size: {} bytes for {}x{} f32",
                data.len(),
                vocab_size,
                dim
            )));
        }

        // Safe conversion from bytes to f32 slice
        let embeddings: Vec<f32> = data
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect();

        Ok(Self {
            tokenizer,
            embeddings,
            vocab_size,
        })
    }

    /// Embed a single text string.
    ///
    /// Returns a 64-dimensional L2-normalized embedding vector.
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        // Tokenize
        let encoding = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| EmbedError(format!("Tokenization failed: {e}")))?;

        let ids = encoding.get_ids();
        if ids.is_empty() {
            // Return zero vector for empty input
            return Ok(vec![0.0; EMBEDDING_DIM]);
        }

        // Mean pool embeddings
        let mut sum = vec![0.0f32; EMBEDDING_DIM];
        let mut count = 0usize;

        for &id in ids {
            let id = id as usize;
            if id < self.vocab_size {
                let offset = id * EMBEDDING_DIM;
                for (i, s) in sum.iter_mut().enumerate() {
                    *s += self.embeddings[offset + i];
                }
                count += 1;
            }
        }

        if count == 0 {
            return Ok(vec![0.0; EMBEDDING_DIM]);
        }

        // Mean (precision loss acceptable for embedding averaging)
        #[allow(clippy::cast_precision_loss)]
        let count_f = count as f32;
        for s in &mut sum {
            *s /= count_f;
        }

        // L2 normalize
        let norm: f32 = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 1e-12 {
            for s in &mut sum {
                *s /= norm;
            }
        }

        Ok(sum)
    }

    /// Embed multiple texts in a batch.
    ///
    /// More efficient than calling `embed()` repeatedly.
    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    /// Get the embedding dimension (64 for potion-base-2M).
    pub const fn dimension(&self) -> usize {
        EMBEDDING_DIM
    }

    /// Get the vocabulary size.
    pub fn vocab_size(&self) -> usize {
        self.vocab_size
    }
}

/// Compute cosine similarity between two embedding vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

    if norm_a < 1e-12 || norm_b < 1e-12 {
        return 0.0;
    }

    dot / (norm_a * norm_b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedder_loads() {
        let embedder = Embedder::global().expect("Failed to load embedder");
        assert_eq!(embedder.dimension(), 64);
        assert!(embedder.vocab_size() > 0);
    }

    #[test]
    fn test_embed_single() {
        let embedder = Embedder::global().expect("Failed to load embedder");
        let embedding = embedder.embed("hello world").expect("Failed to embed");
        assert_eq!(embedding.len(), 64);

        // Check it's normalized (L2 norm should be ~1.0)
        let norm: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 0.01,
            "Expected normalized, got norm={norm}"
        );
    }

    #[test]
    fn test_embed_empty() {
        let embedder = Embedder::global().expect("Failed to load embedder");
        let embedding = embedder.embed("").expect("Failed to embed");
        assert_eq!(embedding.len(), 64);
    }

    #[test]
    fn test_similar_texts() {
        let embedder = Embedder::global().expect("Failed to load embedder");

        let payment1 = embedder.embed("process payment").unwrap();
        let payment2 = embedder.embed("charge credit card").unwrap();
        let search = embedder.embed("search documents").unwrap();

        let sim_related = cosine_similarity(&payment1, &payment2);
        let sim_unrelated = cosine_similarity(&payment1, &search);

        // Related concepts should be more similar
        assert!(
            sim_related > sim_unrelated,
            "Expected payment concepts to be more similar: related={sim_related}, unrelated={sim_unrelated}"
        );
    }

    #[test]
    fn test_cosine_similarity() {
        // Identical vectors = 1.0
        let v = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 0.001);

        // Orthogonal vectors = 0.0
        let v1 = vec![1.0, 0.0];
        let v2 = vec![0.0, 1.0];
        assert!(cosine_similarity(&v1, &v2).abs() < 0.001);

        // Empty = 0.0
        assert!(cosine_similarity(&[], &[]).abs() < f32::EPSILON);
    }

    #[test]
    fn test_embed_batch() {
        let embedder = Embedder::global().expect("Failed to load embedder");
        let texts = &["hello", "world", "test"];
        let embeddings = embedder.embed_batch(texts).expect("Failed to batch embed");

        assert_eq!(embeddings.len(), 3);
        for emb in &embeddings {
            assert_eq!(emb.len(), 64);
        }
    }

    #[test]
    fn test_deterministic() {
        let embedder = Embedder::global().expect("Failed to load embedder");

        let emb1 = embedder.embed("test input").unwrap();
        let emb2 = embedder.embed("test input").unwrap();

        // Same input should produce identical output
        assert_eq!(emb1, emb2);
    }
}
