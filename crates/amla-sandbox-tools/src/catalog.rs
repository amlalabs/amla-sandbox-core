//! Tool catalog with BM25 and semantic search for capability discovery.
//!
//! This module provides:
//! - [`ToolDef`]: Rich tool definition with metadata
//! - [`ToolCatalog`]: In-memory BM25 index for fast tool search
//! - Semantic search via `Model2Vec` embeddings (WASM-compatible)

use std::collections::HashMap;
use std::fmt::Write;

use serde::{Deserialize, Serialize};

#[cfg(feature = "tokenizers")]
use crate::embedder::Embedder;

/// BM25 parameters (standard values).
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Normalize tool name to canonical format (colon separator).
///
/// Tool names can use either dots or colons as separators:
/// - `stripe.charge` → `stripe:charge`
/// - `stripe:charge` → `stripe:charge` (unchanged)
#[inline]
#[must_use]
pub fn normalize_tool_name(name: &str) -> String {
    name.replace('.', ":")
}

/// Rich tool definition for search/discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    /// Unique tool identifier (e.g., "stripe:charge", "notion:search").
    /// Names are stored in canonical format with colon separators.
    pub name: String,

    /// Human-readable description for LLM understanding.
    pub description: String,

    /// Detailed parameter definitions.
    pub parameters: Vec<ParamDef>,

    /// Optional category for filtering (e.g., "payments", "database", "file").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,

    /// Additional keywords for search (not in description).
    #[serde(default)]
    pub keywords: Vec<String>,

    /// Pre-computed embedding vector for semantic search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
}

impl ToolDef {
    /// Create searchable text for BM25 indexing.
    ///
    /// Includes: tool name, description, all parameter details (name, type, description,
    /// required/optional), category, and keywords.
    #[must_use]
    pub fn searchable_text(&self) -> String {
        let mut text = format!("{} {}", self.name.replace(':', " "), self.description);
        for param in &self.parameters {
            let type_str = match param.param_type {
                ParamType::String => "string",
                ParamType::Integer => "integer",
                ParamType::Number => "number",
                ParamType::Boolean => "boolean",
                ParamType::Array => "array",
                ParamType::Object => "object",
            };
            let req_str = if param.required {
                "required"
            } else {
                "optional"
            };
            let _ = write!(
                text,
                " {} {} {} {}",
                param.name, type_str, req_str, param.description
            );
        }
        if let Some(cat) = &self.category {
            let _ = write!(text, " {cat}");
        }
        for kw in &self.keywords {
            let _ = write!(text, " {kw}");
        }
        text
    }
}

/// Parameter definition for a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamDef {
    /// Parameter name.
    pub name: String,

    /// Parameter type.
    pub param_type: ParamType,

    /// Human-readable description.
    pub description: String,

    /// Whether the parameter is required.
    pub required: bool,

    /// Default value (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

impl ParamDef {
    /// Create a new parameter definition.
    pub fn new(
        name: impl Into<String>,
        param_type: ParamType,
        description: impl Into<String>,
        required: bool,
    ) -> Self {
        Self {
            name: name.into(),
            param_type,
            description: description.into(),
            required,
            default: None,
        }
    }

    /// Set a default value.
    #[must_use]
    pub fn with_default(mut self, value: serde_json::Value) -> Self {
        self.default = Some(value);
        self
    }
}

/// Parameter types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ParamType {
    /// String value.
    String,
    /// Integer value.
    Integer,
    /// Floating point number.
    Number,
    /// Boolean value.
    Boolean,
    /// Array of values.
    Array,
    /// Object/struct.
    Object,
}

impl ParamType {
    /// Convert to TypeScript type string.
    #[must_use]
    pub fn to_typescript(&self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer | Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "unknown[]",
            Self::Object => "Record<string, unknown>",
        }
    }
}

impl From<&str> for ParamType {
    fn from(s: &str) -> Self {
        match s {
            "integer" => Self::Integer,
            "number" => Self::Number,
            "boolean" => Self::Boolean,
            "array" => Self::Array,
            "object" => Self::Object,
            _ => Self::String, // Default to string for "string" and unknown types
        }
    }
}

/// Search result with score.
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// The matched tool.
    pub tool: ToolDef,
    /// Relevance score.
    pub score: f64,
}

/// In-memory tool catalog with BM25 index.
#[derive(Clone)]
pub struct ToolCatalog {
    /// All registered tools.
    tools: Vec<ToolDef>,

    /// Tool name -> index lookup.
    name_to_idx: HashMap<String, usize>,

    /// Inverted index: term -> `Vec<(doc_idx, term_freq)>`.
    inverted_index: HashMap<String, Vec<(usize, u32)>>,

    /// Document lengths (token count per doc).
    doc_lengths: Vec<usize>,

    /// Average document length.
    avg_doc_length: f64,

    /// Document frequency: term -> doc count containing term.
    doc_freq: HashMap<String, usize>,
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolCatalog {
    /// Create an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            name_to_idx: HashMap::new(),
            inverted_index: HashMap::new(),
            doc_lengths: Vec::new(),
            avg_doc_length: 0.0,
            doc_freq: HashMap::new(),
        }
    }

    /// Build catalog from tool definitions.
    #[must_use]
    pub fn from_tools(tools: Vec<ToolDef>) -> Self {
        let mut catalog = Self::new();
        for tool in tools {
            catalog.add_tool(tool);
        }
        catalog.compute_statistics();
        catalog
    }

    /// Build catalog with embeddings computed for each tool.
    ///
    /// Uses the bundled `Model2Vec` model (~8MB, 64-dim embeddings).
    ///
    /// Requires the `tokenizers` feature.
    #[cfg(feature = "tokenizers")]
    #[must_use]
    pub fn from_tools_with_embeddings(tools: Vec<ToolDef>) -> Self {
        let Ok(embedder) = Embedder::global() else {
            return Self::from_tools(tools);
        };

        let tools_with_embeddings: Vec<ToolDef> = tools
            .into_iter()
            .map(|mut tool| {
                if let Ok(emb) = embedder.embed(&tool.searchable_text()) {
                    tool.embedding = Some(emb);
                }
                tool
            })
            .collect();

        Self::from_tools(tools_with_embeddings)
    }

    /// Add a tool and index it.
    ///
    /// Tool names are normalized to canonical format (colon separators).
    pub fn add_tool(&mut self, mut tool: ToolDef) {
        // Normalize tool name to canonical format
        tool.name = normalize_tool_name(&tool.name);

        let idx = self.tools.len();
        let text = tool.searchable_text();
        let tokens = Self::tokenize(&text);

        self.doc_lengths.push(tokens.len());

        let mut term_freqs: HashMap<String, u32> = HashMap::new();
        for token in &tokens {
            *term_freqs.entry(token.clone()).or_insert(0) += 1;
        }

        for (term, freq) in term_freqs {
            self.inverted_index
                .entry(term.clone())
                .or_default()
                .push((idx, freq));
            *self.doc_freq.entry(term).or_insert(0) += 1;
        }

        self.name_to_idx.insert(tool.name.clone(), idx);
        self.tools.push(tool);
    }

    #[allow(clippy::cast_precision_loss)]
    fn compute_statistics(&mut self) {
        if !self.doc_lengths.is_empty() {
            let total: usize = self.doc_lengths.iter().sum();
            self.avg_doc_length = total as f64 / self.doc_lengths.len() as f64;
        }
    }

    /// Tokenize text into lowercase terms.
    fn tokenize(text: &str) -> Vec<String> {
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| s.len() > 1)
            .map(String::from)
            .collect()
    }

    /// Search tools using BM25 (keyword search).
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        self.search_bm25(query, limit)
    }

    /// BM25 keyword search.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn search_bm25(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        if self.tools.is_empty() {
            return Vec::new();
        }

        let query_tokens = Self::tokenize(query);
        let n = self.tools.len() as f64;

        let mut scores: Vec<(usize, f64)> = Vec::new();

        for doc_idx in 0..self.tools.len() {
            let doc_len = self.doc_lengths[doc_idx] as f64;
            let mut score = 0.0;

            for token in &query_tokens {
                if let Some(postings) = self.inverted_index.get(token)
                    && let Some((_, tf)) = postings.iter().find(|(idx, _)| *idx == doc_idx)
                {
                    let tf = f64::from(*tf);
                    let df = *self.doc_freq.get(token).unwrap_or(&1) as f64;

                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    let tf_norm = (tf * (K1 + 1.0))
                        / (tf + K1 * (1.0 - B + B * doc_len / self.avg_doc_length.max(1.0)));

                    score += idf * tf_norm;
                }
            }

            if score > 0.0 {
                scores.push((doc_idx, score));
            }
        }

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        scores
            .into_iter()
            .take(limit)
            .map(|(idx, score)| SearchResult {
                tool: self.tools[idx].clone(),
                score,
            })
            .collect()
    }

    /// Semantic search using pre-computed embeddings.
    #[must_use]
    pub fn search_semantic(&self, query_embedding: &[f32], limit: usize) -> Vec<SearchResult> {
        let mut scores: Vec<(usize, f64)> = self
            .tools
            .iter()
            .enumerate()
            .filter_map(|(idx, tool)| {
                tool.embedding.as_ref().map(|emb| {
                    let score = Self::cosine_similarity(query_embedding, emb);
                    (idx, score)
                })
            })
            .collect();

        scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        scores
            .into_iter()
            .take(limit)
            .map(|(idx, score)| SearchResult {
                tool: self.tools[idx].clone(),
                score,
            })
            .collect()
    }

    /// Hybrid search: combines BM25 and semantic scores.
    #[must_use]
    pub fn search_hybrid(
        &self,
        query: &str,
        query_embedding: Option<&[f32]>,
        limit: usize,
        bm25_weight: f64,
    ) -> Vec<SearchResult> {
        let bm25_results = self.search_bm25(query, self.tools.len());
        let max_bm25 = bm25_results.first().map_or(1.0, |r| r.score).max(0.001);

        let mut combined: HashMap<String, (f64, ToolDef)> = HashMap::new();

        for result in bm25_results {
            let norm_score = result.score / max_bm25 * bm25_weight;
            combined.insert(result.tool.name.clone(), (norm_score, result.tool));
        }

        if let Some(emb) = query_embedding {
            let semantic_results = self.search_semantic(emb, self.tools.len());
            let semantic_weight = 1.0 - bm25_weight;

            for result in semantic_results {
                combined
                    .entry(result.tool.name.clone())
                    .and_modify(|(score, _)| *score += result.score * semantic_weight)
                    .or_insert((result.score * semantic_weight, result.tool));
            }
        }

        let mut results: Vec<SearchResult> = combined
            .into_values()
            .map(|(score, tool)| SearchResult { tool, score })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(limit);
        results
    }

    /// Smart search: combines BM25 and semantic search with automatic query embedding.
    ///
    /// Requires the `tokenizers` feature. Without it, use `search_bm25` directly.
    #[cfg(feature = "tokenizers")]
    #[must_use]
    pub fn search_smart(&self, query: &str, limit: usize) -> Vec<SearchResult> {
        let query_emb = Embedder::global().ok().and_then(|e| e.embed(query).ok());

        match query_emb {
            Some(emb) => self.search_hybrid(query, Some(&emb), limit, 0.5),
            None => self.search_bm25(query, limit),
        }
    }

    fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }

        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }

        f64::from(dot / (norm_a * norm_b))
    }

    /// Get tool by name.
    ///
    /// Name is normalized before lookup, so both `stripe.charge` and
    /// `stripe:charge` will find the same tool.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolDef> {
        let normalized = normalize_tool_name(name);
        self.name_to_idx
            .get(&normalized)
            .map(|&idx| &self.tools[idx])
    }

    /// List all tools.
    #[must_use]
    pub fn list(&self) -> &[ToolDef] {
        &self.tools
    }

    /// Get number of tools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if catalog is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Filter catalog to only tools matching a predicate.
    #[must_use]
    pub fn filter<F>(&self, predicate: F) -> ToolCatalog
    where
        F: Fn(&ToolDef) -> bool,
    {
        let filtered: Vec<ToolDef> = self
            .tools
            .iter()
            .filter(|t| predicate(t))
            .cloned()
            .collect();
        ToolCatalog::from_tools(filtered)
    }

    /// Filter by category.
    #[must_use]
    pub fn filter_by_category(&self, category: &str) -> ToolCatalog {
        self.filter(|t| t.category.as_deref() == Some(category))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tools() -> Vec<ToolDef> {
        vec![
            ToolDef {
                name: "stripe:charge".to_string(),
                description: "Create a payment charge using Stripe".to_string(),
                parameters: vec![
                    ParamDef::new("amount", ParamType::Integer, "Amount in cents", true),
                    ParamDef::new("currency", ParamType::String, "Currency code", true),
                ],
                category: Some("payments".to_string()),
                keywords: vec!["payment".to_string(), "credit card".to_string()],
                embedding: None,
            },
            ToolDef {
                name: "stripe:refund".to_string(),
                description: "Refund a previous charge".to_string(),
                parameters: vec![ParamDef::new(
                    "charge_id",
                    ParamType::String,
                    "ID of charge to refund",
                    true,
                )],
                category: Some("payments".to_string()),
                keywords: vec!["refund".to_string()],
                embedding: None,
            },
            ToolDef {
                name: "notion:search".to_string(),
                description: "Search Notion pages and databases".to_string(),
                parameters: vec![
                    ParamDef::new("query", ParamType::String, "Search query", true),
                    ParamDef::new("limit", ParamType::Integer, "Max results", false),
                ],
                category: Some("productivity".to_string()),
                keywords: vec!["documents".to_string(), "notes".to_string()],
                embedding: None,
            },
        ]
    }

    #[test]
    fn test_catalog_creation() {
        let catalog = ToolCatalog::from_tools(sample_tools());
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn test_get_by_name() {
        let catalog = ToolCatalog::from_tools(sample_tools());
        let tool = catalog.get("stripe:charge");
        assert!(tool.is_some());
        assert_eq!(tool.unwrap().name, "stripe:charge");
        assert!(catalog.get("nonexistent").is_none());
    }

    #[test]
    fn test_search_basic() {
        let catalog = ToolCatalog::from_tools(sample_tools());
        let results = catalog.search("payment charge", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:charge");
    }

    #[test]
    fn test_filter_by_category() {
        let catalog = ToolCatalog::from_tools(sample_tools());
        let payments = catalog.filter_by_category("payments");
        assert_eq!(payments.len(), 2);
    }

    #[test]
    fn test_empty_catalog() {
        let catalog = ToolCatalog::new();
        assert!(catalog.is_empty());
        assert_eq!(catalog.search("anything", 10).len(), 0);
    }

    // ========== COMPREHENSIVE SEARCH TESTS ==========

    #[test]
    fn test_search_by_description_terms() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Search by description words
        let results = catalog.search("refund", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:refund");

        // Search by word in description
        let results = catalog.search("databases", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "notion:search");
    }

    #[test]
    fn test_search_by_keyword() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Search by keyword not in description
        let results = catalog.search("credit card", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:charge");

        // Search by another keyword
        let results = catalog.search("notes documents", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "notion:search");
    }

    #[test]
    fn test_search_by_category() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Category is included in searchable text
        let results = catalog.search("payments", 10);
        assert!(results.len() >= 2);

        // Both payment tools should appear
        let names: Vec<&str> = results.iter().map(|r| r.tool.name.as_str()).collect();
        assert!(names.contains(&"stripe:charge"));
        assert!(names.contains(&"stripe:refund"));
    }

    #[test]
    fn test_search_by_param_name() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Search by parameter name
        let results = catalog.search("currency", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:charge");
    }

    #[test]
    fn test_search_by_param_type() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Parameter types are included in searchable text
        let results = catalog.search("integer required", 10);
        assert!(!results.is_empty());
        // stripe:charge has multiple required integer params
    }

    #[test]
    fn test_search_by_tool_name() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Tool name (without colon) is searchable
        let results = catalog.search("notion", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "notion:search");

        let results = catalog.search("stripe", 10);
        assert!(results.len() >= 2);
    }

    #[test]
    fn test_search_multi_term() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Multi-term query should boost relevance
        let results = catalog.search("stripe payment charge amount", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:charge");
    }

    #[test]
    fn test_search_case_insensitive() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Search should be case-insensitive
        let results_lower = catalog.search("stripe", 10);
        let results_upper = catalog.search("STRIPE", 10);
        let results_mixed = catalog.search("StRiPe", 10);

        assert!(!results_lower.is_empty());
        assert_eq!(results_lower.len(), results_upper.len());
        assert_eq!(results_lower.len(), results_mixed.len());
        assert_eq!(results_lower[0].tool.name, results_upper[0].tool.name);
    }

    #[test]
    fn test_search_limit() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Limit should be respected - use "stripe" which matches both stripe tools
        let results_1 = catalog.search("stripe", 1);
        assert_eq!(results_1.len(), 1);

        let results_2 = catalog.search("stripe", 2);
        assert_eq!(results_2.len(), 2);

        // Limit larger than results
        let results_100 = catalog.search("notion", 100);
        assert_eq!(results_100.len(), 1);
    }

    #[test]
    fn test_search_no_match() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let results = catalog.search("xyznonexistent", 10);
        assert!(results.is_empty());

        let results = catalog.search("kubernetes helm docker", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_empty_query() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let results = catalog.search("", 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_special_characters() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Special characters should be handled gracefully
        let results = catalog.search("payment!@#$%", 10);
        // Should still match "payment"
        assert!(!results.is_empty());

        // Colon in search (tool name separator)
        let results = catalog.search("stripe:charge", 10);
        // Colon is tokenized away, so it searches for "stripe" and "charge"
        assert!(!results.is_empty());
    }

    #[test]
    fn test_search_scores_are_sorted() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let results = catalog.search("payment charge", 10);
        assert!(results.len() >= 2);

        // Verify scores are in descending order
        for i in 1..results.len() {
            assert!(
                results[i - 1].score >= results[i].score,
                "Results should be sorted by score descending"
            );
        }
    }

    #[test]
    fn test_search_scores_are_positive() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let results = catalog.search("payment", 10);
        for result in &results {
            assert!(
                result.score > 0.0,
                "Matched results should have positive scores"
            );
        }
    }

    // ========== SEARCHABLE TEXT TESTS ==========

    #[test]
    fn test_searchable_text_includes_all_fields() {
        let tool = ToolDef {
            name: "test:tool".to_string(),
            description: "Test description".to_string(),
            parameters: vec![
                ParamDef::new("param1", ParamType::String, "First param", true),
                ParamDef::new("param2", ParamType::Integer, "Second param", false),
            ],
            category: Some("testing".to_string()),
            keywords: vec!["keyword1".to_string(), "keyword2".to_string()],
            embedding: None,
        };

        let text = tool.searchable_text();

        // Check all components are included
        assert!(text.contains("test tool")); // Name with colon removed
        assert!(text.contains("Test description"));
        assert!(text.contains("param1"));
        assert!(text.contains("string"));
        assert!(text.contains("required"));
        assert!(text.contains("param2"));
        assert!(text.contains("integer"));
        assert!(text.contains("optional"));
        assert!(text.contains("testing")); // Category
        assert!(text.contains("keyword1"));
        assert!(text.contains("keyword2"));
    }

    #[test]
    fn test_searchable_text_no_category() {
        let tool = ToolDef {
            name: "simple:tool".to_string(),
            description: "Simple tool".to_string(),
            parameters: vec![],
            category: None,
            keywords: vec![],
            embedding: None,
        };

        let text = tool.searchable_text();
        assert!(text.contains("simple tool"));
        assert!(text.contains("Simple tool"));
    }

    // ========== FILTER TESTS ==========

    #[test]
    fn test_filter_custom_predicate() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Filter by name prefix
        let stripe_only = catalog.filter(|t| t.name.starts_with("stripe:"));
        assert_eq!(stripe_only.len(), 2);

        // Filter by having at least 2 parameters
        let multi_param = catalog.filter(|t| t.parameters.len() >= 2);
        assert!(!multi_param.is_empty());
    }

    #[test]
    fn test_filter_by_nonexistent_category() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let empty = catalog.filter_by_category("nonexistent");
        assert!(empty.is_empty());
    }

    #[test]
    fn test_filtered_catalog_is_searchable() {
        let catalog = ToolCatalog::from_tools(sample_tools());
        let payments = catalog.filter_by_category("payments");

        // Filtered catalog should still be searchable
        let results = payments.search("refund", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "stripe:refund");

        // Non-matching search in filtered catalog
        let results = payments.search("notion", 10);
        assert!(results.is_empty());
    }

    // ========== TOOL MANAGEMENT TESTS ==========

    #[test]
    fn test_add_tool_incremental() {
        let mut catalog = ToolCatalog::new();

        catalog.add_tool(ToolDef {
            name: "first:tool".to_string(),
            description: "First tool".to_string(),
            parameters: vec![],
            category: None,
            keywords: vec![],
            embedding: None,
        });

        assert_eq!(catalog.len(), 1);

        catalog.add_tool(ToolDef {
            name: "second:tool".to_string(),
            description: "Second tool".to_string(),
            parameters: vec![],
            category: None,
            keywords: vec![],
            embedding: None,
        });

        assert_eq!(catalog.len(), 2);

        // Both should be findable
        assert!(catalog.get("first:tool").is_some());
        assert!(catalog.get("second:tool").is_some());
    }

    #[test]
    fn test_list_returns_all_tools() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        let list = catalog.list();
        assert_eq!(list.len(), 3);

        let names: Vec<&str> = list.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"stripe:charge"));
        assert!(names.contains(&"stripe:refund"));
        assert!(names.contains(&"notion:search"));
    }

    // ========== BM25-SPECIFIC TESTS ==========

    #[test]
    fn test_bm25_tf_boost() {
        // Terms that appear multiple times in a document should boost relevance
        let tools = vec![
            ToolDef {
                name: "payment:tool".to_string(),
                description: "payment payment payment processor".to_string(),
                parameters: vec![],
                category: Some("payment".to_string()),
                keywords: vec!["payment".to_string()],
                embedding: None,
            },
            ToolDef {
                name: "other:tool".to_string(),
                description: "payment once".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: None,
            },
        ];

        let catalog = ToolCatalog::from_tools(tools);
        let results = catalog.search("payment", 10);

        assert_eq!(results.len(), 2);
        // First result should have higher score due to term frequency
        assert!(results[0].score > results[1].score);
        assert_eq!(results[0].tool.name, "payment:tool");
    }

    #[test]
    fn test_bm25_idf_boost() {
        // Rare terms should boost relevance more than common terms
        let tools = vec![
            ToolDef {
                name: "common:tool".to_string(),
                description: "payment tool".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: None,
            },
            ToolDef {
                name: "rare:tool".to_string(),
                description: "payment xyzrareterm".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: None,
            },
            ToolDef {
                name: "another:tool".to_string(),
                description: "payment processor".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: None,
            },
        ];

        let catalog = ToolCatalog::from_tools(tools);

        // Searching for the rare term should find only one doc with high score
        let results = catalog.search("xyzrareterm", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool.name, "rare:tool");
    }

    // ========== SEMANTIC SEARCH TESTS ==========

    #[test]
    fn test_semantic_search_with_embeddings() {
        // Create tools with mock embeddings
        let tools = vec![
            ToolDef {
                name: "payment:tool".to_string(),
                description: "Process payments".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: Some(vec![1.0, 0.0, 0.0]), // Unit vector in x direction
            },
            ToolDef {
                name: "search:tool".to_string(),
                description: "Search documents".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: Some(vec![0.0, 1.0, 0.0]), // Unit vector in y direction
            },
        ];

        let catalog = ToolCatalog::from_tools(tools);

        // Query embedding similar to payment tool
        let query_emb = vec![0.9, 0.1, 0.0];
        let results = catalog.search_semantic(&query_emb, 10);

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].tool.name, "payment:tool");
        assert!(results[0].score > results[1].score);
    }

    #[test]
    fn test_semantic_search_no_embeddings() {
        let catalog = ToolCatalog::from_tools(sample_tools()); // No embeddings

        let query_emb = vec![1.0, 0.0, 0.0];
        let results = catalog.search_semantic(&query_emb, 10);

        // No results since tools have no embeddings
        assert!(results.is_empty());
    }

    #[test]
    fn test_cosine_similarity() {
        // Same vector = 1.0
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        let sim = ToolCatalog::cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 0.001);

        // Orthogonal vectors = 0.0
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = ToolCatalog::cosine_similarity(&a, &b);
        assert!(sim.abs() < 0.001);

        // Opposite vectors = -1.0
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![-1.0, 0.0, 0.0];
        let sim = ToolCatalog::cosine_similarity(&a, &b);
        assert!((sim + 1.0).abs() < 0.001);
    }

    #[test]
    #[allow(clippy::float_cmp)] // Edge cases return exact 0.0
    fn test_cosine_similarity_edge_cases() {
        // Empty vectors
        let a: Vec<f32> = vec![];
        let b: Vec<f32> = vec![];
        assert_eq!(ToolCatalog::cosine_similarity(&a, &b), 0.0);

        // Different lengths
        let a = vec![1.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(ToolCatalog::cosine_similarity(&a, &b), 0.0);

        // Zero vector
        let a = vec![0.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert_eq!(ToolCatalog::cosine_similarity(&a, &b), 0.0);
    }

    // ========== HYBRID SEARCH TESTS ==========

    #[test]
    fn test_hybrid_search() {
        let tools = vec![
            ToolDef {
                name: "payment:charge".to_string(),
                description: "Create a payment charge".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: Some(vec![1.0, 0.0, 0.0]),
            },
            ToolDef {
                name: "payment:refund".to_string(),
                description: "Refund a charge".to_string(),
                parameters: vec![],
                category: None,
                keywords: vec![],
                embedding: Some(vec![0.8, 0.2, 0.0]),
            },
        ];

        let catalog = ToolCatalog::from_tools(tools);

        // Hybrid search with BM25 weight 0.5
        let query_emb = vec![1.0, 0.0, 0.0];
        let results = catalog.search_hybrid("charge", Some(&query_emb), 10, 0.5);

        assert!(!results.is_empty());
        // "charge" appears in first tool's description, and embedding matches
        assert_eq!(results[0].tool.name, "payment:charge");
    }

    #[test]
    fn test_hybrid_search_no_embedding() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Hybrid search without embedding falls back to pure BM25
        let results = catalog.search_hybrid("payment", None, 10, 0.5);

        assert!(!results.is_empty());
    }

    // ========== PARAM DEF TESTS ==========

    #[test]
    fn test_param_def_with_default() {
        let param = ParamDef::new("limit", ParamType::Integer, "Max results", false)
            .with_default(serde_json::json!(10));

        assert_eq!(param.name, "limit");
        assert_eq!(param.param_type, ParamType::Integer);
        assert!(!param.required);
        assert_eq!(param.default, Some(serde_json::json!(10)));
    }

    #[test]
    fn test_param_type_serde() {
        // Test serialization
        let param_type = ParamType::String;
        let json = serde_json::to_string(&param_type).unwrap();
        assert_eq!(json, "\"string\"");

        // Test deserialization
        let param_type: ParamType = serde_json::from_str("\"integer\"").unwrap();
        assert_eq!(param_type, ParamType::Integer);

        // All variants
        assert_eq!(
            serde_json::from_str::<ParamType>("\"number\"").unwrap(),
            ParamType::Number
        );
        assert_eq!(
            serde_json::from_str::<ParamType>("\"boolean\"").unwrap(),
            ParamType::Boolean
        );
        assert_eq!(
            serde_json::from_str::<ParamType>("\"array\"").unwrap(),
            ParamType::Array
        );
        assert_eq!(
            serde_json::from_str::<ParamType>("\"object\"").unwrap(),
            ParamType::Object
        );
    }

    // ========== TOOL DEF SERDE TESTS ==========

    #[test]
    fn test_tool_def_serde_roundtrip() {
        let tool = ToolDef {
            name: "test:tool".to_string(),
            description: "Test tool".to_string(),
            parameters: vec![
                ParamDef::new("id", ParamType::String, "ID", true),
                ParamDef::new("count", ParamType::Integer, "Count", false)
                    .with_default(serde_json::json!(10)),
            ],
            category: Some("testing".to_string()),
            keywords: vec!["test".to_string()],
            embedding: None,
        };

        let json = serde_json::to_string(&tool).unwrap();
        let parsed: ToolDef = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.name, tool.name);
        assert_eq!(parsed.description, tool.description);
        assert_eq!(parsed.parameters.len(), 2);
        assert_eq!(parsed.category, tool.category);
        assert_eq!(parsed.keywords, tool.keywords);
    }

    #[test]
    fn test_tool_def_serde_optional_fields() {
        // Minimal tool (no category, no keywords, no embedding)
        let json = r#"{
            "name": "minimal:tool",
            "description": "Minimal",
            "parameters": []
        }"#;

        let tool: ToolDef = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "minimal:tool");
        assert!(tool.category.is_none());
        assert!(tool.keywords.is_empty());
        assert!(tool.embedding.is_none());
    }

    // ========== ADDITIONAL COVERAGE TESTS ==========

    #[test]
    fn test_searchable_text_all_param_types() {
        // Test all ParamType variants are included in searchable text
        let tool = ToolDef {
            name: "all:types".to_string(),
            description: "Tool with all parameter types".to_string(),
            parameters: vec![
                ParamDef::new("str_param", ParamType::String, "String parameter", true),
                ParamDef::new("int_param", ParamType::Integer, "Integer parameter", true),
                ParamDef::new("num_param", ParamType::Number, "Number parameter", true),
                ParamDef::new("bool_param", ParamType::Boolean, "Boolean parameter", true),
                ParamDef::new("arr_param", ParamType::Array, "Array parameter", false),
                ParamDef::new("obj_param", ParamType::Object, "Object parameter", false),
            ],
            category: None,
            keywords: vec![],
            embedding: None,
        };

        let text = tool.searchable_text();

        // Verify all type names appear in searchable text
        assert!(text.contains("string"), "Should contain 'string'");
        assert!(text.contains("integer"), "Should contain 'integer'");
        assert!(text.contains("number"), "Should contain 'number'");
        assert!(text.contains("boolean"), "Should contain 'boolean'");
        assert!(text.contains("array"), "Should contain 'array'");
        assert!(text.contains("object"), "Should contain 'object'");

        // Verify required/optional status
        assert!(text.contains("required"), "Should contain 'required'");
        assert!(text.contains("optional"), "Should contain 'optional'");
    }

    #[test]
    fn test_search_by_all_param_types() {
        let tool = ToolDef {
            name: "typed:tool".to_string(),
            description: "Tool with typed parameters".to_string(),
            parameters: vec![
                ParamDef::new("data", ParamType::Number, "Numeric data", true),
                ParamDef::new("flag", ParamType::Boolean, "Boolean flag", false),
                ParamDef::new("items", ParamType::Array, "Array of items", false),
                ParamDef::new("config", ParamType::Object, "Config object", false),
            ],
            category: None,
            keywords: vec![],
            embedding: None,
        };

        let catalog = ToolCatalog::from_tools(vec![tool]);

        // Search by parameter type names
        let results = catalog.search("number", 10);
        assert!(!results.is_empty());
        assert_eq!(results[0].tool.name, "typed:tool");

        let results = catalog.search("boolean", 10);
        assert!(!results.is_empty());

        let results = catalog.search("array", 10);
        assert!(!results.is_empty());

        let results = catalog.search("object", 10);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_tool_catalog_default() {
        // Test Default::default() implementation
        let catalog: ToolCatalog = ToolCatalog::default();
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);

        // Should behave same as new()
        let catalog_new = ToolCatalog::new();
        assert_eq!(catalog.len(), catalog_new.len());
        assert!(catalog.list().is_empty());
    }

    #[test]
    fn test_filter_returns_searchable_catalog() {
        let catalog = ToolCatalog::from_tools(sample_tools());

        // Filter to a subset
        let filtered = catalog.filter(|t| t.parameters.len() == 1);

        // Filtered catalog should be valid and searchable
        assert!(!filtered.is_empty());

        // Should be able to search the filtered catalog
        let results = filtered.search("refund", 10);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_param_type_from_str() {
        // Known types
        assert_eq!(ParamType::from("string"), ParamType::String);
        assert_eq!(ParamType::from("integer"), ParamType::Integer);
        assert_eq!(ParamType::from("number"), ParamType::Number);
        assert_eq!(ParamType::from("boolean"), ParamType::Boolean);
        assert_eq!(ParamType::from("array"), ParamType::Array);
        assert_eq!(ParamType::from("object"), ParamType::Object);

        // Unknown types default to String
        assert_eq!(ParamType::from("unknown"), ParamType::String);
        assert_eq!(ParamType::from(""), ParamType::String);
        assert_eq!(ParamType::from("custom_type"), ParamType::String);
    }

    #[test]
    fn test_param_type_to_typescript() {
        assert_eq!(ParamType::String.to_typescript(), "string");
        assert_eq!(ParamType::Integer.to_typescript(), "number");
        assert_eq!(ParamType::Number.to_typescript(), "number");
        assert_eq!(ParamType::Boolean.to_typescript(), "boolean");
        assert_eq!(ParamType::Array.to_typescript(), "unknown[]");
        assert_eq!(ParamType::Object.to_typescript(), "Record<string, unknown>");
    }

    #[test]
    fn test_param_type_roundtrip() {
        // Parse and convert to TypeScript in one step
        assert_eq!(ParamType::from("integer").to_typescript(), "number");
        assert_eq!(ParamType::from("number").to_typescript(), "number");
        assert_eq!(ParamType::from("string").to_typescript(), "string");
        assert_eq!(ParamType::from("boolean").to_typescript(), "boolean");
        assert_eq!(ParamType::from("array").to_typescript(), "unknown[]");
        assert_eq!(
            ParamType::from("object").to_typescript(),
            "Record<string, unknown>"
        );

        // Unknown types -> String -> "string"
        assert_eq!(ParamType::from("weird").to_typescript(), "string");
    }
}
