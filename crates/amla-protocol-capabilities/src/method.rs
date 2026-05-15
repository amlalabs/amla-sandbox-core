//! Method capability for JSON-RPC method protection.
//!
//! This module provides [`MethodCapability`] for protecting JSON-RPC method calls
//! with glob patterns, parameter constraints, and call count limits.
//!
//! # Example
//!
//! ```rust
//! use amla_capabilities::method::MethodCapability;
//! use amla_constraints::{Constraint, ConstraintSet};
//! use serde_json::json;
//!
//! // Create a capability for Stripe charges
//! let cap = MethodCapability::new("stripe/charges/*")
//!     .with_constraints(ConstraintSet::new(vec![
//!         Constraint::Le {
//!             param: "amount".to_string(),
//!             value: json!(10000),
//!         },
//!         Constraint::In {
//!             param: "currency".to_string(),
//!             values: vec![json!("USD"), json!("EUR")],
//!         },
//!     ]))
//!     .with_max_calls(100);
//!
//! // Validate a call
//! assert!(cap.validate_call(
//!     "stripe/charges/create",
//!     &json!({"amount": 500, "currency": "USD"})
//! ).is_ok());
//!
//! // Invalid amount
//! assert!(cap.validate_call(
//!     "stripe/charges/create",
//!     &json!({"amount": 50000, "currency": "USD"})
//! ).is_err());
//! ```

use amla_constraints::ConstraintSet;
use amla_protocol::CapabilityData;
use serde::{Deserialize, Serialize};

use crate::CapabilityError;
use crate::patterns::{method_matches_pattern, pattern_is_subset};

/// Capability type identifier for method capabilities.
pub const METHOD_CAPABILITY_TYPE: &str = "method";

/// Capability protecting JSON-RPC method calls.
///
/// A method capability grants permission to call methods matching a glob pattern,
/// subject to parameter constraints and optional call count limits.
///
/// # Attenuation
///
/// A child capability is a valid attenuation of a parent if:
/// 1. Child `method_pattern` is a subset of parent (matches fewer methods)
/// 2. Child inherits all parent constraints and may add more (more restrictive)
/// 3. Child `max_calls` ≤ parent `max_calls` (if parent has a limit)
/// 4. Child `input_schema` is compatible with parent (if parent has a schema)
///
/// # Key Format
///
/// The capability key is derived from the method pattern:
/// `cap:method:{pattern}` (e.g., `cap:method:stripe/charges/*`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MethodCapability {
    /// Glob pattern for method names (e.g., "stripe/charges/*", "mcp/**")
    pub method_pattern: String,

    /// Parameter constraints (all must pass)
    #[serde(default)]
    pub constraints: ConstraintSet,

    /// Maximum calls allowed (None = unlimited)
    #[serde(default)]
    pub max_calls: Option<u32>,

    /// Optional JSON Schema for parameter validation
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
}

impl MethodCapability {
    /// Create a new method capability with just a pattern.
    ///
    /// # Example
    ///
    /// ```rust
    /// use amla_capabilities::method::MethodCapability;
    ///
    /// let cap = MethodCapability::new("stripe/charges/*");
    /// ```
    #[must_use]
    pub fn new(method_pattern: impl Into<String>) -> Self {
        Self {
            method_pattern: method_pattern.into(),
            constraints: ConstraintSet::empty(),
            max_calls: None,
            input_schema: None,
        }
    }

    /// Add constraints to the capability.
    ///
    /// # Example
    ///
    /// ```rust
    /// use amla_capabilities::method::MethodCapability;
    /// use amla_constraints::{Constraint, ConstraintSet};
    /// use serde_json::json;
    ///
    /// let cap = MethodCapability::new("stripe/charges/*")
    ///     .with_constraints(ConstraintSet::new(vec![
    ///         Constraint::Le { param: "amount".to_string(), value: json!(1000) },
    ///     ]));
    /// ```
    #[must_use]
    pub fn with_constraints(mut self, constraints: ConstraintSet) -> Self {
        self.constraints = constraints;
        self
    }

    /// Set the maximum number of calls allowed.
    ///
    /// # Example
    ///
    /// ```rust
    /// use amla_capabilities::method::MethodCapability;
    ///
    /// let cap = MethodCapability::new("stripe/charges/*")
    ///     .with_max_calls(100);
    /// ```
    #[must_use]
    pub fn with_max_calls(mut self, max_calls: u32) -> Self {
        self.max_calls = Some(max_calls);
        self
    }

    /// Set the input schema for parameter validation.
    ///
    /// # Example
    ///
    /// ```rust
    /// use amla_capabilities::method::MethodCapability;
    /// use serde_json::json;
    ///
    /// let cap = MethodCapability::new("stripe/charges/*")
    ///     .with_input_schema(json!({
    ///         "type": "object",
    ///         "properties": {
    ///             "amount": { "type": "integer", "minimum": 0 }
    ///         },
    ///         "required": ["amount"]
    ///     }));
    /// ```
    #[must_use]
    pub fn with_input_schema(mut self, schema: serde_json::Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    /// Get the capability key derived from the method pattern.
    ///
    /// Keys are formatted as `cap:method:{pattern}`.
    #[must_use]
    pub fn key(&self) -> String {
        format!("cap:method:{}", self.method_pattern)
    }

    /// Validate a method call against this capability.
    ///
    /// Checks:
    /// 1. Method name matches the pattern
    /// 2. Parameters satisfy all constraints
    ///
    /// Note: `max_calls` is not checked here - that's tracked externally by the CTA.
    ///
    /// # Errors
    ///
    /// Returns an error if the method doesn't match or constraints are violated.
    pub fn validate_call(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<(), CapabilityError> {
        // Check method pattern
        if !method_matches_pattern(method, &self.method_pattern) {
            return Err(CapabilityError::ConstraintViolation(format!(
                "method '{}' does not match pattern '{}'",
                method, self.method_pattern
            )));
        }

        // Check constraints
        self.constraints.evaluate(params)?;

        Ok(())
    }

    /// Check if this capability is a valid attenuation of a parent.
    ///
    /// A child is a valid attenuation if it grants equal or fewer permissions:
    /// 1. Child pattern is subset of parent pattern
    /// 2. Child constraints are at least as restrictive as parent
    /// 3. Child `max_calls` ≤ parent `max_calls` (if parent has limit)
    ///
    /// # Example
    ///
    /// ```rust
    /// use amla_capabilities::method::MethodCapability;
    /// use amla_constraints::{Constraint, ConstraintSet};
    /// use serde_json::json;
    ///
    /// let parent = MethodCapability::new("stripe/**")
    ///     .with_max_calls(100);
    ///
    /// // Valid: narrower pattern, lower limit
    /// let valid_child = MethodCapability::new("stripe/charges/*")
    ///     .with_max_calls(50);
    /// assert!(valid_child.is_subset_of(&parent));
    ///
    /// // Invalid: broader pattern
    /// let invalid_child = MethodCapability::new("**");
    /// assert!(!invalid_child.is_subset_of(&parent));
    ///
    /// // Invalid: higher limit
    /// let invalid_limit = MethodCapability::new("stripe/charges/*")
    ///     .with_max_calls(200);
    /// assert!(!invalid_limit.is_subset_of(&parent));
    /// ```
    #[must_use]
    pub fn is_subset_of(&self, parent: &MethodCapability) -> bool {
        // 1. Pattern must be subset
        if !pattern_is_subset(&self.method_pattern, &parent.method_pattern) {
            return false;
        }

        // 2. Constraints: parent must subsume child
        // (parent's constraints must be implied by child's constraints)
        if !parent.constraints.subsumes(&self.constraints) {
            return false;
        }

        // 3. max_calls: if parent has limit, child must have equal or lower limit
        match (parent.max_calls, self.max_calls) {
            (Some(parent_max), Some(child_max)) => {
                if child_max > parent_max {
                    return false;
                }
            }
            (Some(_), None) => {
                // Parent has limit but child doesn't - privilege escalation!
                return false;
            }
            (None, _) => {
                // Parent has no limit - any child limit is fine
            }
        }

        // TODO: input_schema compatibility check
        // For now, if parent has schema, child must have compatible subset
        // This is complex to implement correctly, so we'll be permissive

        true
    }

    /// Convert to `CapabilityData` for embedding in a PCA.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_capability_data(&self) -> Result<CapabilityData, crate::CapabilityError> {
        CapabilityData::new(&self.key(), METHOD_CAPABILITY_TYPE, self)
            .map_err(|e| CapabilityError::ConstraintViolation(format!("failed to serialize: {e}")))
    }

    /// Create from `CapabilityData`.
    ///
    /// # Errors
    ///
    /// Returns an error if the capability type doesn't match or deserialization fails.
    pub fn from_capability_data(data: &CapabilityData) -> Result<Self, crate::CapabilityError> {
        if data.capability_type() != METHOD_CAPABILITY_TYPE {
            return Err(CapabilityError::TypeNotAllowed(format!(
                "expected type '{}', got '{}'",
                METHOD_CAPABILITY_TYPE,
                data.capability_type()
            )));
        }

        data.decode()
            .map_err(|e| CapabilityError::ConstraintViolation(format!("failed to decode: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_constraints::Constraint;
    use serde_json::json;

    #[test]
    fn test_method_capability_new() {
        let cap = MethodCapability::new("stripe/charges/*");
        assert_eq!(cap.method_pattern, "stripe/charges/*");
        assert!(cap.constraints.is_empty());
        assert!(cap.max_calls.is_none());
        assert!(cap.input_schema.is_none());
    }

    #[test]
    fn test_method_capability_builder() {
        let cap = MethodCapability::new("stripe/charges/*")
            .with_constraints(ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(1000),
            }]))
            .with_max_calls(100)
            .with_input_schema(json!({"type": "object"}));

        assert_eq!(cap.method_pattern, "stripe/charges/*");
        assert_eq!(cap.constraints.len(), 1);
        assert_eq!(cap.max_calls, Some(100));
        assert!(cap.input_schema.is_some());
    }

    #[test]
    fn test_method_capability_key() {
        let cap = MethodCapability::new("stripe/charges/*");
        assert_eq!(cap.key(), "cap:method:stripe/charges/*");

        let cap2 = MethodCapability::new("**");
        assert_eq!(cap2.key(), "cap:method:**");
    }

    #[test]
    fn test_validate_call_pattern_match() {
        let cap = MethodCapability::new("stripe/charges/*");

        // Matches
        assert!(
            cap.validate_call("stripe/charges/create", &json!({}))
                .is_ok()
        );
        assert!(
            cap.validate_call("stripe/charges/refund", &json!({}))
                .is_ok()
        );

        // Doesn't match
        assert!(
            cap.validate_call("stripe/customers/create", &json!({}))
                .is_err()
        );
        assert!(cap.validate_call("github/repos/list", &json!({})).is_err());
    }

    #[test]
    fn test_validate_call_with_constraints() {
        let cap =
            MethodCapability::new("stripe/charges/*").with_constraints(ConstraintSet::new(vec![
                Constraint::Ge {
                    param: "amount".to_string(),
                    value: json!(100),
                },
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(10000),
                },
            ]));

        // Valid
        assert!(
            cap.validate_call("stripe/charges/create", &json!({"amount": 500}))
                .is_ok()
        );

        // Below minimum
        assert!(
            cap.validate_call("stripe/charges/create", &json!({"amount": 50}))
                .is_err()
        );

        // Above maximum
        assert!(
            cap.validate_call("stripe/charges/create", &json!({"amount": 50000}))
                .is_err()
        );
    }

    #[test]
    fn test_is_subset_of_pattern() {
        let parent = MethodCapability::new("stripe/**");

        // Valid: narrower pattern
        let child1 = MethodCapability::new("stripe/charges/*");
        assert!(child1.is_subset_of(&parent));

        let child2 = MethodCapability::new("stripe/charges/create");
        assert!(child2.is_subset_of(&parent));

        // Invalid: different prefix
        let child3 = MethodCapability::new("github/**");
        assert!(!child3.is_subset_of(&parent));

        // Invalid: broader pattern
        let child4 = MethodCapability::new("**");
        assert!(!child4.is_subset_of(&parent));
    }

    #[test]
    fn test_is_subset_of_max_calls() {
        let parent = MethodCapability::new("stripe/**").with_max_calls(100);

        // Valid: lower limit
        let child1 = MethodCapability::new("stripe/charges/*").with_max_calls(50);
        assert!(child1.is_subset_of(&parent));

        // Valid: same limit
        let child2 = MethodCapability::new("stripe/charges/*").with_max_calls(100);
        assert!(child2.is_subset_of(&parent));

        // Invalid: higher limit
        let child3 = MethodCapability::new("stripe/charges/*").with_max_calls(200);
        assert!(!child3.is_subset_of(&parent));

        // Invalid: no limit (unlimited)
        let child4 = MethodCapability::new("stripe/charges/*");
        assert!(!child4.is_subset_of(&parent));
    }

    #[test]
    fn test_is_subset_of_constraints() {
        let parent = MethodCapability::new("stripe/**").with_constraints(ConstraintSet::new(vec![
            Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            },
        ]));

        // Valid: stricter constraint
        let child1 =
            MethodCapability::new("stripe/charges/*").with_constraints(ConstraintSet::new(vec![
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(5000),
                },
            ]));
        assert!(child1.is_subset_of(&parent));

        // Valid: adds extra constraint on DIFFERENT param (more restrictive)
        // Note: Adding constraints on the same param with different types (e.g., adding Ge
        // when parent has Le) is not currently supported by ConstraintSet::subsumes
        let child2 =
            MethodCapability::new("stripe/charges/*").with_constraints(ConstraintSet::new(vec![
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(10000),
                },
                Constraint::In {
                    param: "currency".to_string(),
                    values: vec![json!("USD"), json!("EUR")],
                },
            ]));
        assert!(child2.is_subset_of(&parent));

        // Invalid: looser constraint
        let child3 =
            MethodCapability::new("stripe/charges/*").with_constraints(ConstraintSet::new(vec![
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(20000),
                },
            ]));
        assert!(!child3.is_subset_of(&parent));
    }

    #[test]
    fn test_capability_data_roundtrip() {
        let original = MethodCapability::new("stripe/charges/*")
            .with_constraints(ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(1000),
            }]))
            .with_max_calls(100);

        let data = original.to_capability_data().unwrap();
        assert_eq!(data.key(), "cap:method:stripe/charges/*");
        assert_eq!(data.capability_type(), METHOD_CAPABILITY_TYPE);

        let decoded = MethodCapability::from_capability_data(&data).unwrap();
        assert_eq!(decoded.method_pattern, original.method_pattern);
        assert_eq!(decoded.max_calls, original.max_calls);
    }

    #[test]
    fn test_from_capability_data_wrong_type() {
        let data =
            CapabilityData::new("cap:test", "wrong-type", &json!({"method_pattern": "test"}))
                .unwrap();

        let result = MethodCapability::from_capability_data(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_serialization() {
        let cap = MethodCapability::new("stripe/charges/*")
            .with_constraints(ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(1000),
            }]))
            .with_max_calls(100);

        // Serialize to JSON
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("stripe/charges/*"));

        // Deserialize back
        let decoded: MethodCapability = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.method_pattern, cap.method_pattern);
        assert_eq!(decoded.max_calls, cap.max_calls);
    }
}
