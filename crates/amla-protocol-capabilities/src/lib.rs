//! Capability types for AI agent sandboxing.
//!
//! This crate provides concrete capability types for the amla runtime:

#![forbid(unsafe_code)]
//!
//! - [`ToolCallCap`] - Call a tool with parameter constraints
//! - [`MemoryReadCap`] - Read from partitioned storage
//! - [`MemoryWriteCap`] - Write to partitioned storage
//! - [`MemoryDeleteCap`] - Delete from partitioned storage
//! - [`SpawnCap`] - Spawn child sessions with attenuated capabilities
//! - [`method::MethodCapability`] - JSON-RPC method protection with glob patterns
//!
//! ## Method Capabilities (JSON-RPC Protection)
//!
//! The [`method::MethodCapability`] type protects JSON-RPC method calls with
//! glob patterns, parameter constraints, and call count limits:
//!
//! ```rust
//! use amla_capabilities::method::MethodCapability;
//! use amla_constraints::{Constraint, ConstraintSet};
//! use serde_json::json;
//!
//! let cap = MethodCapability::new("stripe/charges/*")
//!     .with_constraints(ConstraintSet::new(vec![
//!         Constraint::Le { param: "amount".to_string(), value: json!(10000) },
//!     ]))
//!     .with_max_calls(100);
//!
//! // Validate a call
//! assert!(cap.validate_call("stripe/charges/create", &json!({"amount": 500})).is_ok());
//! ```
//!
//! ## Partition Patterns
//!
//! Memory capabilities use [`PartitionPattern`] for path-based access control:
//!
//! ```rust
//! use amla_capabilities::PartitionPattern;
//!
//! // Recursive pattern - matches all keys under tenant/alice/
//! let pattern = PartitionPattern::new("tenant/alice/**");
//! assert!(pattern.matches("tenant/alice/foo"));
//! assert!(pattern.matches("tenant/alice/foo/bar"));
//!
//! // Non-recursive - only direct children
//! let pattern = PartitionPattern::new("tenant/alice/*");
//! assert!(pattern.matches("tenant/alice/foo"));
//! assert!(!pattern.matches("tenant/alice/foo/bar"));
//! ```
//!
//! ## Tool Call Constraints
//!
//! Tool calls can be constrained using the [`amla_constraints`] DSL:
//!
//! ```rust
//! use amla_capabilities::ToolCallCap;
//! use amla_constraints::{Constraint, ConstraintSet};
//! use serde_json::json;
//!
//! let cap = ToolCallCap::with_constraints(
//!     "stripe:charge",
//!     ConstraintSet::new(vec![
//!         Constraint::Ge { param: "amount".to_string(), value: json!(100) },
//!         Constraint::Le { param: "amount".to_string(), value: json!(10000) },
//!     ]),
//! );
//!
//! // Valid parameters
//! assert!(cap.check(&json!({"amount": 500})).is_ok());
//!
//! // Invalid - below minimum
//! assert!(cap.check(&json!({"amount": 50})).is_err());
//! ```

// missing_docs lint inherited from workspace
#![deny(rustdoc::broken_intra_doc_links)]

// New modules for method capabilities
pub mod method;
pub mod patterns;
pub mod validator;

#[cfg(feature = "wasm")]
pub mod wasm;

use amla_constraints::{ConstraintError, ConstraintSet};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Error types for capability operations.
#[derive(Debug, Error)]
pub enum CapabilityError {
    /// Constraint was violated during capability check
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),

    /// Access to partition was denied
    #[error("Partition access denied: {0}")]
    PartitionDenied(String),

    /// Capability type is not allowed
    #[error("Capability type not allowed: {0}")]
    TypeNotAllowed(String),
}

impl From<ConstraintError> for CapabilityError {
    fn from(e: ConstraintError) -> Self {
        CapabilityError::ConstraintViolation(e.to_string())
    }
}

/// Common trait for capability types.
pub trait Capability: Serialize + for<'de> Deserialize<'de> {
    /// Capability type identifier (e.g., "tool-call", "memory-read").
    fn cap_type() -> &'static str;

    /// Check if this capability is a superset of another (for attenuation).
    ///
    /// Returns `true` if `self` allows everything that `child` allows.
    fn is_superset_of(&self, child: &Self) -> bool;
}

// ============================================================================
// Partition Pattern
// ============================================================================

/// Pattern mode for partition matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PatternMode {
    /// Only exact match
    Exact,
    /// Match prefix and direct children (one level)
    NonRecursive,
    /// Match prefix and all descendants
    Recursive,
}

/// Partition pattern for path-based access control.
///
/// Patterns are prefix-based for O(1) containment checking:
///
/// - `tenant/alice/**` - Recursive, matches `tenant/alice/` and all descendants
/// - `tenant/alice/*` - Non-recursive, matches prefix and direct children only
/// - `tenant/alice` - Exact match only
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionPattern {
    /// The path prefix
    pub prefix: String,
    /// Matching mode
    pub mode: PatternMode,
}

impl PartitionPattern {
    /// Create a new partition pattern from a glob-like string.
    ///
    /// - `path/**` - Recursive pattern (matches path and all descendants)
    /// - `path/*` - Non-recursive pattern (matches path and direct children)
    /// - `path` - Exact match only
    pub fn new(pattern: &str) -> Self {
        if let Some(prefix) = pattern.strip_suffix("/**") {
            Self {
                prefix: prefix.to_string(),
                mode: PatternMode::Recursive,
            }
        } else if let Some(prefix) = pattern.strip_suffix("/*") {
            Self {
                prefix: prefix.to_string(),
                mode: PatternMode::NonRecursive,
            }
        } else {
            Self {
                prefix: pattern.to_string(),
                mode: PatternMode::Exact,
            }
        }
    }

    /// Check if a key matches this pattern.
    pub fn matches(&self, key: &str) -> bool {
        // Exact match is always valid for any mode
        if key == self.prefix {
            return true;
        }

        // For exact mode, only the prefix itself matches
        if self.mode == PatternMode::Exact {
            return false;
        }

        // The key must start with prefix + "/"
        let expected_prefix = format!("{}/", self.prefix);
        if !key.starts_with(&expected_prefix) {
            return false;
        }

        if self.mode == PatternMode::Recursive {
            return true;
        }

        // Non-recursive: no additional slashes after prefix/
        let remainder = &key[expected_prefix.len()..];
        !remainder.contains('/')
    }

    /// Check if this pattern contains another (for attenuation).
    ///
    /// Returns `true` if everything matched by `child` is also matched by `self`.
    pub fn contains(&self, child: &Self) -> bool {
        // Child prefix must start with our prefix
        if !child.prefix.starts_with(&self.prefix) {
            return false;
        }

        // If we're recursive, we contain everything under us
        // But we must enforce segment boundaries to prevent prefix collisions
        // e.g., "user/alice/**" should NOT contain "user/alicexxx/**"
        if self.mode == PatternMode::Recursive {
            // Child prefix must be exactly ours, or start with ours + "/"
            if child.prefix == self.prefix {
                return true;
            }
            let expected_prefix = format!("{}/", self.prefix);
            return child.prefix.starts_with(&expected_prefix);
        }

        // Exact parent can only contain exact child with same prefix
        if self.mode == PatternMode::Exact {
            return child.prefix == self.prefix && child.mode == PatternMode::Exact;
        }

        // Non-recursive parent can contain:
        // - Exact child at most one level deeper (same depth rule as matches())
        // - Non-recursive child with same prefix
        match child.mode {
            PatternMode::Exact => {
                // Child must be exactly one level deeper (no extra slashes)
                // e.g., "tenant/alice/*" contains "tenant/alice/foo" but NOT "tenant/alice/foo/bar"
                // Note: same prefix doesn't work because non-recursive requires one more segment
                let expected_prefix = format!("{}/", self.prefix);
                if !child.prefix.starts_with(&expected_prefix) {
                    return false;
                }
                let remainder = &child.prefix[expected_prefix.len()..];
                !remainder.contains('/')
            }
            PatternMode::NonRecursive => child.prefix == self.prefix,
            PatternMode::Recursive => false, // Can't contain recursive child
        }
    }
}

// ============================================================================
// Tool Call Capability
// ============================================================================

/// Capability to call a specific tool with parameter constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallCap {
    /// Tool identifier (e.g., `stripe:charge`, `github:create_issue`)
    pub tool: String,
    /// Parameter constraints
    pub constraints: ConstraintSet,
}

impl ToolCallCap {
    /// Create a new tool call capability with no constraints.
    pub fn new(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            constraints: ConstraintSet::empty(),
        }
    }

    /// Create a new tool call capability with constraints.
    pub fn with_constraints(tool: impl Into<String>, constraints: ConstraintSet) -> Self {
        Self {
            tool: tool.into(),
            constraints,
        }
    }

    /// Check if parameters satisfy the constraints.
    pub fn check(&self, params: &serde_json::Value) -> Result<(), CapabilityError> {
        self.constraints.evaluate(params)?;
        Ok(())
    }
}

impl Capability for ToolCallCap {
    fn cap_type() -> &'static str {
        "tool-call"
    }

    fn is_superset_of(&self, child: &Self) -> bool {
        self.tool == child.tool && self.constraints.subsumes(&child.constraints)
    }
}

// ============================================================================
// Memory Capabilities
// ============================================================================

/// Capability to read from partitioned storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryReadCap {
    /// Partition pattern for allowed keys
    pub partition: PartitionPattern,
}

impl MemoryReadCap {
    /// Create a new memory read capability.
    pub fn new(pattern: &str) -> Self {
        Self {
            partition: PartitionPattern::new(pattern),
        }
    }

    /// Check if a key is allowed for reading.
    pub fn check(&self, key: &str) -> Result<(), CapabilityError> {
        if self.partition.matches(key) {
            Ok(())
        } else {
            Err(CapabilityError::PartitionDenied(key.to_string()))
        }
    }
}

impl Capability for MemoryReadCap {
    fn cap_type() -> &'static str {
        "memory-read"
    }

    fn is_superset_of(&self, child: &Self) -> bool {
        self.partition.contains(&child.partition)
    }
}

/// Capability to write to partitioned storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryWriteCap {
    /// Partition pattern for allowed keys
    pub partition: PartitionPattern,
    /// Maximum value size in bytes (None = unlimited)
    pub max_value_bytes: Option<u64>,
}

impl MemoryWriteCap {
    /// Create a new memory write capability.
    pub fn new(pattern: &str) -> Self {
        Self {
            partition: PartitionPattern::new(pattern),
            max_value_bytes: None,
        }
    }

    /// Create a memory write capability with a size limit.
    pub fn with_max_bytes(pattern: &str, max_bytes: u64) -> Self {
        Self {
            partition: PartitionPattern::new(pattern),
            max_value_bytes: Some(max_bytes),
        }
    }

    /// Check if a key and value size are allowed for writing.
    pub fn check(&self, key: &str, value_size: usize) -> Result<(), CapabilityError> {
        if !self.partition.matches(key) {
            return Err(CapabilityError::PartitionDenied(key.to_string()));
        }
        if let Some(max) = self.max_value_bytes
            && value_size as u64 > max
        {
            return Err(CapabilityError::ConstraintViolation(format!(
                "Value size {value_size} exceeds maximum {max}"
            )));
        }
        Ok(())
    }
}

impl Capability for MemoryWriteCap {
    fn cap_type() -> &'static str {
        "memory-write"
    }

    fn is_superset_of(&self, child: &Self) -> bool {
        if !self.partition.contains(&child.partition) {
            return false;
        }
        match (self.max_value_bytes, child.max_value_bytes) {
            (None, _) => true,            // No limit contains any limit
            (Some(_), None) => false,     // Limited cannot contain unlimited
            (Some(p), Some(c)) => p >= c, // Parent limit must be >= child limit
        }
    }
}

/// Capability to delete from partitioned storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDeleteCap {
    /// Partition pattern for allowed keys
    pub partition: PartitionPattern,
}

impl MemoryDeleteCap {
    /// Create a new memory delete capability.
    pub fn new(pattern: &str) -> Self {
        Self {
            partition: PartitionPattern::new(pattern),
        }
    }

    /// Check if a key is allowed for deletion.
    pub fn check(&self, key: &str) -> Result<(), CapabilityError> {
        if self.partition.matches(key) {
            Ok(())
        } else {
            Err(CapabilityError::PartitionDenied(key.to_string()))
        }
    }
}

impl Capability for MemoryDeleteCap {
    fn cap_type() -> &'static str {
        "memory-delete"
    }

    fn is_superset_of(&self, child: &Self) -> bool {
        self.partition.contains(&child.partition)
    }
}

// ============================================================================
// Spawn Capability
// ============================================================================

/// Capability to spawn child sessions with attenuated capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnCap {
    /// Partition pattern prefix for child sessions
    pub partition_prefix: PartitionPattern,
    /// Allowed capability types for spawned children
    pub allowed_types: Vec<String>,
}

impl SpawnCap {
    /// Create a new spawn capability.
    pub fn new(pattern: &str, allowed_types: Vec<String>) -> Self {
        Self {
            partition_prefix: PartitionPattern::new(pattern),
            allowed_types,
        }
    }

    /// Check if spawning with the given capability type is allowed.
    pub fn check_type(&self, cap_type: &str) -> Result<(), CapabilityError> {
        if self.allowed_types.contains(&cap_type.to_string()) {
            Ok(())
        } else {
            Err(CapabilityError::TypeNotAllowed(cap_type.to_string()))
        }
    }
}

impl Capability for SpawnCap {
    fn cap_type() -> &'static str {
        "spawn"
    }

    fn is_superset_of(&self, child: &Self) -> bool {
        // Child must be within our partition
        if !self.partition_prefix.contains(&child.partition_prefix) {
            return false;
        }
        // Child's allowed types must be a subset of ours
        child
            .allowed_types
            .iter()
            .all(|t| self.allowed_types.contains(t))
    }
}

// ============================================================================
// Capability Set
// ============================================================================

/// A collection of capabilities for a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitySet {
    /// Tool call capabilities
    pub tool_calls: Vec<ToolCallCap>,
    /// Memory read capabilities
    pub memory_reads: Vec<MemoryReadCap>,
    /// Memory write capabilities
    pub memory_writes: Vec<MemoryWriteCap>,
    /// Memory delete capabilities
    pub memory_deletes: Vec<MemoryDeleteCap>,
    /// Spawn capabilities
    pub spawns: Vec<SpawnCap>,
}

impl CapabilitySet {
    /// Create an empty capability set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tool call capability.
    #[must_use]
    pub fn add_tool_call(mut self, cap: ToolCallCap) -> Self {
        self.tool_calls.push(cap);
        self
    }

    /// Add a memory read capability.
    #[must_use]
    pub fn add_memory_read(mut self, cap: MemoryReadCap) -> Self {
        self.memory_reads.push(cap);
        self
    }

    /// Add a memory write capability.
    #[must_use]
    pub fn add_memory_write(mut self, cap: MemoryWriteCap) -> Self {
        self.memory_writes.push(cap);
        self
    }

    /// Add a memory delete capability.
    #[must_use]
    pub fn add_memory_delete(mut self, cap: MemoryDeleteCap) -> Self {
        self.memory_deletes.push(cap);
        self
    }

    /// Add a spawn capability.
    #[must_use]
    pub fn add_spawn(mut self, cap: SpawnCap) -> Self {
        self.spawns.push(cap);
        self
    }

    /// Check if a tool call is allowed.
    pub fn check_tool_call(
        &self,
        tool: &str,
        params: &serde_json::Value,
    ) -> Result<(), CapabilityError> {
        for cap in &self.tool_calls {
            if cap.tool == tool {
                return cap.check(params);
            }
        }
        Err(CapabilityError::TypeNotAllowed(format!(
            "No capability for tool: {tool}"
        )))
    }

    /// Check if a memory read is allowed.
    pub fn check_memory_read(&self, key: &str) -> Result<(), CapabilityError> {
        for cap in &self.memory_reads {
            if cap.check(key).is_ok() {
                return Ok(());
            }
        }
        Err(CapabilityError::PartitionDenied(key.to_string()))
    }

    /// Check if a memory write is allowed.
    pub fn check_memory_write(&self, key: &str, value_size: usize) -> Result<(), CapabilityError> {
        for cap in &self.memory_writes {
            if cap.check(key, value_size).is_ok() {
                return Ok(());
            }
        }
        Err(CapabilityError::PartitionDenied(key.to_string()))
    }

    /// Check if a memory delete is allowed.
    pub fn check_memory_delete(&self, key: &str) -> Result<(), CapabilityError> {
        for cap in &self.memory_deletes {
            if cap.check(key).is_ok() {
                return Ok(());
            }
        }
        Err(CapabilityError::PartitionDenied(key.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_constraints::Constraint;
    use serde_json::json;

    // ========================================================================
    // Partition Pattern Tests
    // ========================================================================

    #[test]
    fn test_partition_pattern_matches_recursive() {
        let pattern = PartitionPattern::new("tenant/alice/**");
        assert!(pattern.matches("tenant/alice"));
        assert!(pattern.matches("tenant/alice/foo"));
        assert!(pattern.matches("tenant/alice/foo/bar"));
        assert!(pattern.matches("tenant/alice/foo/bar/baz"));
        assert!(!pattern.matches("tenant/bob/foo"));
        assert!(!pattern.matches("tenant/alicex"));
    }

    #[test]
    fn test_partition_pattern_matches_non_recursive() {
        let pattern = PartitionPattern::new("tenant/alice/*");
        assert!(pattern.matches("tenant/alice"));
        assert!(pattern.matches("tenant/alice/foo"));
        assert!(!pattern.matches("tenant/alice/foo/bar"));
    }

    #[test]
    fn test_partition_pattern_matches_exact() {
        let pattern = PartitionPattern::new("tenant/alice");
        assert!(pattern.matches("tenant/alice"));
        assert!(!pattern.matches("tenant/alice/foo"));
    }

    #[test]
    fn test_partition_pattern_contains() {
        let parent = PartitionPattern::new("tenant/alice/**");
        let child = PartitionPattern::new("tenant/alice/scratch/**");
        assert!(parent.contains(&child));
        assert!(!child.contains(&parent));

        // Non-recursive cannot contain recursive
        let parent = PartitionPattern::new("tenant/alice/*");
        let child = PartitionPattern::new("tenant/alice/scratch/**");
        assert!(!parent.contains(&child));
    }

    #[test]
    fn test_partition_pattern_contains_non_recursive_depth_limit() {
        // Regression test: non-recursive parent should NOT contain
        // exact children that are more than one level deeper
        let parent = PartitionPattern::new("tenant/alice/*");

        // Should contain exact child one level deeper
        let child = PartitionPattern::new("tenant/alice/foo");
        assert!(parent.contains(&child));

        // Should NOT contain exact child two levels deeper (this was the bug!)
        let child = PartitionPattern::new("tenant/alice/foo/bar");
        assert!(
            !parent.contains(&child),
            "non-recursive 'tenant/alice/*' should not contain exact 'tenant/alice/foo/bar'"
        );

        // Should NOT contain exact child three levels deeper
        let child = PartitionPattern::new("tenant/alice/foo/bar/baz");
        assert!(!parent.contains(&child));

        // Should NOT contain exact child with same prefix (no segment added)
        let child = PartitionPattern::new("tenant/alice");
        assert!(
            !parent.contains(&child),
            "non-recursive 'tenant/alice/*' should not contain exact 'tenant/alice' (needs one segment)"
        );
    }

    #[test]
    fn test_partition_pattern_contains_consistency_with_matches() {
        // The contains() logic should be consistent with matches()
        let parent = PartitionPattern::new("tenant/alice/*");

        // "tenant/alice/*" matches "tenant/alice/foo" - so should contain its exact
        assert!(parent.matches("tenant/alice/foo"));
        assert!(parent.contains(&PartitionPattern::new("tenant/alice/foo")));

        // "tenant/alice/*" does NOT match "tenant/alice/foo/bar" - so should NOT contain
        assert!(!parent.matches("tenant/alice/foo/bar"));
        assert!(!parent.contains(&PartitionPattern::new("tenant/alice/foo/bar")));
    }

    #[test]
    fn test_partition_pattern_contains_recursive_segment_boundary() {
        // Regression test: recursive parent must enforce segment boundaries
        // to prevent prefix collision attacks
        let parent = PartitionPattern::new("user/alice/**");

        // Should contain patterns that are actually under user/alice/
        assert!(parent.contains(&PartitionPattern::new("user/alice/**")));
        assert!(parent.contains(&PartitionPattern::new("user/alice/*")));
        assert!(parent.contains(&PartitionPattern::new("user/alice")));
        assert!(parent.contains(&PartitionPattern::new("user/alice/data/**")));
        assert!(parent.contains(&PartitionPattern::new("user/alice/data/secrets/**")));

        // Should NOT contain patterns with similar but different prefixes
        // This was the bug: "user/alice" prefix-matches "user/alicexxx"
        assert!(
            !parent.contains(&PartitionPattern::new("user/alicexxx/**")),
            "user/alice/** should NOT contain user/alicexxx/** (different user!)"
        );
        assert!(
            !parent.contains(&PartitionPattern::new("user/alice_backup/**")),
            "user/alice/** should NOT contain user/alice_backup/**"
        );
        assert!(
            !parent.contains(&PartitionPattern::new("user/alicesmith/**")),
            "user/alice/** should NOT contain user/alicesmith/**"
        );

        // Edge case: completely unrelated prefix
        assert!(!parent.contains(&PartitionPattern::new("admin/bob/**")));
    }

    // ========================================================================
    // Tool Call Capability Tests
    // ========================================================================

    #[test]
    fn test_tool_call_cap_no_constraints() {
        let cap = ToolCallCap::new("test:tool");
        assert!(cap.check(&json!({"any": "params"})).is_ok());
    }

    #[test]
    fn test_tool_call_cap_with_constraints() {
        let cap = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![
                Constraint::Ge {
                    param: "amount".to_string(),
                    value: json!(100),
                },
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(10000),
                },
            ]),
        );

        assert!(cap.check(&json!({"amount": 500})).is_ok());
        assert!(cap.check(&json!({"amount": 100})).is_ok());
        assert!(cap.check(&json!({"amount": 10000})).is_ok());
        assert!(cap.check(&json!({"amount": 50})).is_err());
        assert!(cap.check(&json!({"amount": 50000})).is_err());
    }

    #[test]
    fn test_tool_call_cap_superset() {
        let parent = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            }]),
        );

        // Child with stricter constraint
        let child = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(5000),
            }]),
        );
        assert!(parent.is_superset_of(&child));

        // Child with looser constraint
        let child = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(20000),
            }]),
        );
        assert!(!parent.is_superset_of(&child));

        // Different tool
        let other_tool = ToolCallCap::new("other:tool");
        assert!(!parent.is_superset_of(&other_tool));
    }

    // ========================================================================
    // Memory Capability Tests
    // ========================================================================

    #[test]
    fn test_memory_read_cap() {
        let cap = MemoryReadCap::new("user/alice/**");
        assert!(cap.check("user/alice/prefs").is_ok());
        assert!(cap.check("user/alice/data/settings").is_ok());
        assert!(cap.check("user/bob/prefs").is_err());
    }

    #[test]
    fn test_memory_write_cap_with_limit() {
        let cap = MemoryWriteCap::with_max_bytes("user/alice/**", 1024);
        assert!(cap.check("user/alice/prefs", 100).is_ok());
        assert!(cap.check("user/alice/prefs", 1024).is_ok());
        assert!(cap.check("user/alice/prefs", 2000).is_err());
        assert!(cap.check("user/bob/prefs", 100).is_err());
    }

    #[test]
    fn test_memory_write_cap_superset() {
        let parent = MemoryWriteCap::with_max_bytes("user/**", 1024);

        // Stricter limit
        let child = MemoryWriteCap::with_max_bytes("user/alice/**", 512);
        assert!(parent.is_superset_of(&child));

        // Looser limit
        let child = MemoryWriteCap::with_max_bytes("user/alice/**", 2048);
        assert!(!parent.is_superset_of(&child));

        // Unlimited child
        let child = MemoryWriteCap::new("user/alice/**");
        assert!(!parent.is_superset_of(&child));

        // Unlimited parent
        let parent = MemoryWriteCap::new("user/**");
        let child = MemoryWriteCap::with_max_bytes("user/alice/**", 1024);
        assert!(parent.is_superset_of(&child));
    }

    #[test]
    fn test_memory_delete_cap() {
        let cap = MemoryDeleteCap::new("user/alice/**");
        assert!(cap.check("user/alice/prefs").is_ok());
        assert!(cap.check("user/bob/prefs").is_err());
    }

    // ========================================================================
    // Spawn Capability Tests
    // ========================================================================

    #[test]
    fn test_spawn_cap() {
        let cap = SpawnCap::new(
            "sessions/alice/**",
            vec!["tool-call".to_string(), "memory-read".to_string()],
        );

        assert!(cap.check_type("tool-call").is_ok());
        assert!(cap.check_type("memory-read").is_ok());
        assert!(cap.check_type("memory-write").is_err());
    }

    #[test]
    fn test_spawn_cap_superset() {
        let parent = SpawnCap::new(
            "sessions/**",
            vec![
                "tool-call".to_string(),
                "memory-read".to_string(),
                "memory-write".to_string(),
            ],
        );

        // Valid child - subset of types, within partition
        let child = SpawnCap::new(
            "sessions/alice/**",
            vec!["tool-call".to_string(), "memory-read".to_string()],
        );
        assert!(parent.is_superset_of(&child));

        // Invalid - has type not in parent
        let child = SpawnCap::new(
            "sessions/alice/**",
            vec!["tool-call".to_string(), "spawn".to_string()],
        );
        assert!(!parent.is_superset_of(&child));

        // Invalid - outside partition
        let child = SpawnCap::new("other/**", vec!["tool-call".to_string()]);
        assert!(!parent.is_superset_of(&child));
    }

    // ========================================================================
    // Capability Set Tests
    // ========================================================================

    #[test]
    fn test_capability_set() {
        let caps = CapabilitySet::new()
            .add_tool_call(ToolCallCap::new("stripe:charge"))
            .add_memory_read(MemoryReadCap::new("user/**"))
            .add_memory_write(MemoryWriteCap::new("user/alice/**"));

        assert!(caps.check_tool_call("stripe:charge", &json!({})).is_ok());
        assert!(caps.check_tool_call("other:tool", &json!({})).is_err());

        assert!(caps.check_memory_read("user/alice/prefs").is_ok());
        assert!(caps.check_memory_read("admin/settings").is_err());

        assert!(caps.check_memory_write("user/alice/prefs", 100).is_ok());
        assert!(caps.check_memory_write("user/bob/prefs", 100).is_err());
    }
}

// ============================================================================
// Property-based Tests (proptest)
// ============================================================================

#[cfg(all(test, not(target_arch = "wasm32")))]
mod proptests {
    use super::*;
    use amla_constraints::Constraint;
    use proptest::prelude::*;
    use serde_json::json;

    // Strategy for generating valid path segments (alphanumeric + limited special chars)
    fn path_segment() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9_-]{0,15}".prop_filter("non-empty", |s| !s.is_empty())
    }

    // Strategy for generating partition paths (1-4 segments)
    fn partition_path() -> impl Strategy<Value = String> {
        prop::collection::vec(path_segment(), 1..=4).prop_map(|segments| segments.join("/"))
    }

    // Strategy for generating partition patterns with modes
    fn partition_pattern_strategy() -> impl Strategy<Value = PartitionPattern> {
        (partition_path(), prop::sample::select(vec!["", "*", "**"])).prop_map(|(path, suffix)| {
            let pattern = if suffix.is_empty() {
                path
            } else {
                format!("{path}/{suffix}")
            };
            PartitionPattern::new(&pattern)
        })
    }

    // ========================================================================
    // PartitionPattern Property Tests
    // ========================================================================

    proptest! {
        /// A pattern always matches its own prefix
        #[test]
        fn pattern_matches_own_prefix(pattern in partition_pattern_strategy()) {
            // Exact and non-recursive patterns should match their prefix
            // Recursive patterns match prefix and everything under
            prop_assert!(pattern.matches(&pattern.prefix));
        }

        /// Recursive pattern matches everything that non-recursive matches
        #[test]
        fn recursive_subsumes_non_recursive(path in partition_path()) {
            let recursive = PartitionPattern::new(&format!("{path}/**"));
            let non_recursive = PartitionPattern::new(&format!("{path}/*"));
            let exact = PartitionPattern::new(&path);

            // Recursive contains non-recursive and exact with same prefix
            prop_assert!(recursive.contains(&non_recursive));
            prop_assert!(recursive.contains(&exact));
        }

        /// Non-recursive does not contain recursive
        #[test]
        fn non_recursive_not_contains_recursive(path in partition_path()) {
            let recursive = PartitionPattern::new(&format!("{path}/**"));
            let non_recursive = PartitionPattern::new(&format!("{path}/*"));

            prop_assert!(!non_recursive.contains(&recursive));
        }

        /// Exact only contains itself
        #[test]
        fn exact_only_contains_itself(path in partition_path()) {
            let exact = PartitionPattern::new(&path);
            let recursive = PartitionPattern::new(&format!("{path}/**"));
            let non_recursive = PartitionPattern::new(&format!("{path}/*"));

            prop_assert!(exact.contains(&exact));
            prop_assert!(!exact.contains(&recursive));
            prop_assert!(!exact.contains(&non_recursive));
        }

        /// Segment boundary security: pattern should not match similar prefixes
        #[test]
        fn segment_boundary_enforced(
            base in path_segment(),
            suffix in "[a-z]{1,5}"
        ) {
            let parent = PartitionPattern::new(&format!("user/{base}/**"));
            let attacker = PartitionPattern::new(&format!("user/{base}{suffix}/**"));

            // Parent should NOT contain attacker pattern (different user!)
            prop_assert!(
                !parent.contains(&attacker),
                "user/{base}/** should not contain user/{base}{suffix}/**"
            );

            // And attacker's prefix should not match parent pattern
            prop_assert!(
                !parent.matches(&format!("user/{base}{suffix}/data")),
                "user/{base}/** should not match user/{base}{suffix}/data"
            );
        }

        /// Contains is transitive: if A contains B and B contains C, then A contains C
        #[test]
        fn contains_is_transitive(path in partition_path()) {
            let a = PartitionPattern::new(&format!("{path}/**"));
            let b = PartitionPattern::new(&format!("{path}/sub/**"));
            let c = PartitionPattern::new(&format!("{path}/sub/deep/**"));

            if a.contains(&b) && b.contains(&c) {
                prop_assert!(a.contains(&c));
            }
        }

        /// Contains is reflexive: pattern always contains itself
        #[test]
        fn contains_is_reflexive(pattern in partition_pattern_strategy()) {
            prop_assert!(pattern.contains(&pattern));
        }
    }

    // ========================================================================
    // MemoryWriteCap Property Tests
    // ========================================================================

    proptest! {
        /// Unlimited parent contains any child with same or narrower partition
        #[test]
        fn unlimited_parent_contains_limited_child(
            path in partition_path(),
            max_bytes in 1u64..1_000_000
        ) {
            let parent = MemoryWriteCap::new(&format!("{path}/**"));
            let child = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), max_bytes);

            prop_assert!(parent.is_superset_of(&child));
        }

        /// Limited parent does not contain unlimited child
        #[test]
        fn limited_parent_not_contains_unlimited(
            path in partition_path(),
            max_bytes in 1u64..1_000_000
        ) {
            let parent = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), max_bytes);
            let child = MemoryWriteCap::new(&format!("{path}/**"));

            prop_assert!(!parent.is_superset_of(&child));
        }

        /// Parent with higher limit contains child with lower limit
        #[test]
        fn higher_limit_contains_lower(
            path in partition_path(),
            parent_limit in 1000u64..1_000_000,
            child_limit in 1u64..1000
        ) {
            let parent = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), parent_limit);
            let child = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), child_limit);

            prop_assert!(parent.is_superset_of(&child));
        }

        /// Parent with lower limit does not contain child with higher limit
        #[test]
        fn lower_limit_not_contains_higher(
            path in partition_path(),
            parent_limit in 1u64..1000,
            child_limit in 1000u64..1_000_000
        ) {
            let parent = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), parent_limit);
            let child = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), child_limit);

            prop_assert!(!parent.is_superset_of(&child));
        }

        /// Check respects byte limit
        #[test]
        fn check_respects_byte_limit(
            path in partition_path(),
            max_bytes in 100u64..10000,
            write_bytes in 1usize..20000
        ) {
            let cap = MemoryWriteCap::with_max_bytes(&format!("{path}/**"), max_bytes);
            let key = format!("{path}/test");
            let result = cap.check(&key, write_bytes);

            if write_bytes as u64 <= max_bytes {
                prop_assert!(result.is_ok());
            } else {
                prop_assert!(result.is_err());
            }
        }
    }

    // ========================================================================
    // ToolCallCap Property Tests
    // ========================================================================

    proptest! {
        /// Tool with no constraints accepts any params
        #[test]
        fn no_constraints_accepts_all(tool in "[a-z]+:[a-z]+") {
            let cap = ToolCallCap::new(&tool);
            let empty = json!({});
            let simple = json!({"any": "value"});
            let nested = json!({"nested": {"deep": 123}});
            prop_assert!(cap.check(&empty).is_ok());
            prop_assert!(cap.check(&simple).is_ok());
            prop_assert!(cap.check(&nested).is_ok());
        }

        /// Tool with Le constraint rejects higher values
        #[test]
        fn le_constraint_enforced(
            tool in "[a-z]+:[a-z]+",
            limit in 1i64..1000,
            value in 0i64..2000
        ) {
            let limit_json = json!(limit);
            let cap = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Le {
                    param: "amount".to_string(),
                    value: limit_json,
                }]),
            );

            let params = json!({"amount": value});
            let result = cap.check(&params);
            if value <= limit {
                prop_assert!(result.is_ok());
            } else {
                prop_assert!(result.is_err());
            }
        }

        /// Tool with Ge constraint rejects lower values
        #[test]
        fn ge_constraint_enforced(
            tool in "[a-z]+:[a-z]+",
            limit in 1i64..1000,
            value in 0i64..2000
        ) {
            let limit_json = json!(limit);
            let cap = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Ge {
                    param: "amount".to_string(),
                    value: limit_json,
                }]),
            );

            let params = json!({"amount": value});
            let result = cap.check(&params);
            if value >= limit {
                prop_assert!(result.is_ok());
            } else {
                prop_assert!(result.is_err());
            }
        }

        /// Stricter child is superset of looser parent
        #[test]
        fn stricter_le_is_subset(
            tool in "[a-z]+:[a-z]+",
            parent_limit in 500i64..1000,
            child_limit in 1i64..500
        ) {
            let parent_limit_json = json!(parent_limit);
            let child_limit_json = json!(child_limit);
            let parent = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Le {
                    param: "amount".to_string(),
                    value: parent_limit_json,
                }]),
            );
            let child = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Le {
                    param: "amount".to_string(),
                    value: child_limit_json,
                }]),
            );

            prop_assert!(parent.is_superset_of(&child));
        }

        /// Looser child is NOT superset of stricter parent
        #[test]
        fn looser_le_not_subset(
            tool in "[a-z]+:[a-z]+",
            parent_limit in 1i64..500,
            child_limit in 500i64..1000
        ) {
            let parent_limit_json = json!(parent_limit);
            let child_limit_json = json!(child_limit);
            let parent = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Le {
                    param: "amount".to_string(),
                    value: parent_limit_json,
                }]),
            );
            let child = ToolCallCap::with_constraints(
                &tool,
                ConstraintSet::new(vec![Constraint::Le {
                    param: "amount".to_string(),
                    value: child_limit_json,
                }]),
            );

            prop_assert!(!parent.is_superset_of(&child));
        }

        /// Different tools are never supersets of each other
        #[test]
        fn different_tools_not_superset(
            tool1 in "[a-z]+:[a-z]+",
            tool2 in "[a-z]+:[a-z]+"
        ) {
            prop_assume!(tool1 != tool2);

            let cap1 = ToolCallCap::new(&tool1);
            let cap2 = ToolCallCap::new(&tool2);

            prop_assert!(!cap1.is_superset_of(&cap2));
            prop_assert!(!cap2.is_superset_of(&cap1));
        }
    }

    // ========================================================================
    // SpawnCap Property Tests
    // ========================================================================

    proptest! {
        /// Parent with more allowed types contains child with fewer
        #[test]
        fn more_types_contains_fewer(path in partition_path()) {
            let parent = SpawnCap::new(
                &format!("{path}/**"),
                vec!["tool-call".to_string(), "memory-read".to_string(), "memory-write".to_string()],
            );
            let child = SpawnCap::new(
                &format!("{path}/**"),
                vec!["tool-call".to_string()],
            );

            prop_assert!(parent.is_superset_of(&child));
        }

        /// Child with type not in parent is not contained
        #[test]
        fn child_extra_type_not_contained(path in partition_path()) {
            let parent = SpawnCap::new(
                &format!("{path}/**"),
                vec!["tool-call".to_string()],
            );
            let child = SpawnCap::new(
                &format!("{path}/**"),
                vec!["tool-call".to_string(), "spawn".to_string()],
            );

            prop_assert!(!parent.is_superset_of(&child));
        }
    }
}
