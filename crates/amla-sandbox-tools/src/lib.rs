//! Tool catalog with BM25 and semantic search for AI agent capability discovery.
//!
//! This crate provides:
//! - [`ToolCatalog`]: BM25 + semantic search for tool discovery
//! - [`ToolDef`]: Rich tool definitions with parameters and metadata
//! - [`Embedder`]: `Model2Vec` text embedder (requires `tokenizers` feature)
//!
//! # Features
//!
//! - `tokenizers` - Enable semantic search via `Model2Vec` embeddings (~8MB bundled model).
//!   Without this feature, only BM25 keyword search is available.
//!
//! # Example
//!
//! ```ignore
//! use amla_tools::{ToolCatalog, ToolDef, ParamDef, ParamType};
//!
//! let tools = vec![
//!     ToolDef {
//!         name: "stripe:charge".to_string(),
//!         description: "Create a payment charge".to_string(),
//!         parameters: vec![
//!             ParamDef::new("amount", ParamType::Integer, "Amount in cents", true),
//!         ],
//!         category: Some("payments".to_string()),
//!         keywords: vec!["payment".to_string()],
//!         embedding: None,
//!     },
//! ];
//!
//! // Create catalog (use from_tools_with_embeddings with "tokenizers" feature)
//! let catalog = ToolCatalog::from_tools(tools);
//!
//! // BM25 keyword search (always available)
//! let results = catalog.search("process credit card", 5);
//!
//! // Smart search with semantic (requires "tokenizers" feature)
//! #[cfg(feature = "tokenizers")]
//! let results = catalog.search_smart("process credit card", 5);
//! ```

pub mod catalog;
#[cfg(feature = "tokenizers")]
pub mod embedder;

pub use catalog::{ParamDef, ParamType, SearchResult, ToolCatalog, ToolDef, normalize_tool_name};
#[cfg(feature = "tokenizers")]
pub use embedder::{EMBEDDING_DIM, EmbedError, Embedder, cosine_similarity};
