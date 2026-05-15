//! # amla-constraints
//!
//! Constraint system for capability enforcement.

#![forbid(unsafe_code)]
//!
//! ## Design Principles
//!
//! - **Attenuation is conjunction**: Attenuating adds new clauses AND-ed to existing ones
//! - **Or allowed within clauses**: Individual constraints can use Or for flexibility
//! - **No regex**: Regex containment is undecidable, use `StartsWith`, `EndsWith`, `Contains` instead
//! - **Fail-closed**: Missing parameters cause constraint failure
//!
//! ## Constraint Types
//!
//! | Type | Description | Example |
//! |------|-------------|---------|
//! | `Lt` | Less than | `amount < 100` |
//! | `Le` | Less than or equal | `amount <= 100` |
//! | `Gt` | Greater than | `amount > 0` |
//! | `Ge` | Greater than or equal | `amount >= 1` |
//! | `Eq` | Equal | `currency == "USD"` |
//! | `Ne` | Not equal | `status != "deleted"` |
//! | `In` | Set membership | `currency in ["USD", "EUR"]` |
//! | `NotIn` | Set exclusion | `method not in ["DELETE"]` |
//! | `StartsWith` | String prefix | `path starts with "/api/"` |
//! | `EndsWith` | String suffix | `file ends with ".json"` |
//! | `Contains` | String contains | `query contains "SELECT"` |
//! | `Exists` | Parameter exists | `customer_id exists` |
//! | `NotExists` | Parameter absent | `deprecated not exists` |
//!
//! ## Example
//!
//! ```rust
//! use amla_constraints::{Constraint, ConstraintSet};
//! use serde_json::json;
//!
//! // Create constraints
//! let constraints = ConstraintSet::new(vec![
//!     Constraint::Ge { param: "amount".into(), value: json!(100) },
//!     Constraint::Le { param: "amount".into(), value: json!(10000) },
//!     Constraint::In { param: "currency".into(), values: vec![json!("USD"), json!("EUR")] },
//! ]);
//!
//! // Evaluate against parameters
//! let params = json!({"amount": 500, "currency": "USD"});
//! assert!(constraints.evaluate(&params).is_ok());
//!
//! // Violation
//! let bad_params = json!({"amount": 50, "currency": "USD"});
//! assert!(constraints.evaluate(&bad_params).is_err());
//! ```
//!
//! ## Attenuation (Subsumption)
//!
//! Child constraints can only be **more restrictive** than parent constraints:
//!
//! ```rust
//! use amla_constraints::{Constraint, ConstraintSet};
//! use serde_json::json;
//!
//! let parent = ConstraintSet::new(vec![
//!     Constraint::Le { param: "amount".into(), value: json!(100) },
//! ]);
//!
//! // Child can make constraint stricter (50 <= 100)
//! let valid_child = ConstraintSet::new(vec![
//!     Constraint::Le { param: "amount".into(), value: json!(50) },
//! ]);
//! assert!(parent.subsumes(&valid_child));
//!
//! // Child cannot make constraint looser (200 > 100)
//! let invalid_child = ConstraintSet::new(vec![
//!     Constraint::Le { param: "amount".into(), value: json!(200) },
//! ]);
//! assert!(!parent.subsumes(&invalid_child));
//! ```

// missing_docs lint inherited from workspace
#![deny(rustdoc::broken_intra_doc_links)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Constraint evaluation error.
#[derive(Debug, Error)]
pub enum ConstraintError {
    /// Required parameter is missing.
    #[error("Missing parameter: {0}")]
    MissingParam(String),

    /// Parameter has wrong type.
    #[error("Type mismatch for {param}: expected {expected}, got {actual}")]
    TypeMismatch {
        /// Parameter name that had wrong type.
        param: String,
        /// Expected type name.
        expected: String,
        /// Actual type name.
        actual: String,
    },

    /// Constraint check failed.
    #[error("Constraint violation: {param} {rule}, actual: {actual}")]
    Violation {
        /// Parameter name that violated constraint.
        param: String,
        /// Description of the constraint rule.
        rule: String,
        /// Actual value that violated the constraint.
        actual: String,
    },
}

/// Atomic constraint on a single parameter.
///
/// Constraints are evaluated against JSON parameters. Each constraint type
/// has specific evaluation semantics documented below.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Constraint {
    // Comparison
    /// Parameter must be less than value.
    Lt {
        /// Parameter name to compare.
        param: String,
        /// Threshold value (parameter must be less than this).
        value: Value,
    },
    /// Parameter must be less than or equal to value.
    Le {
        /// Parameter name to compare.
        param: String,
        /// Threshold value (parameter must be at most this).
        value: Value,
    },
    /// Parameter must be greater than value.
    Gt {
        /// Parameter name to compare.
        param: String,
        /// Threshold value (parameter must be greater than this).
        value: Value,
    },
    /// Parameter must be greater than or equal to value.
    Ge {
        /// Parameter name to compare.
        param: String,
        /// Threshold value (parameter must be at least this).
        value: Value,
    },
    /// Parameter must equal value.
    Eq {
        /// Parameter name to compare.
        param: String,
        /// Value that parameter must equal.
        value: Value,
    },
    /// Parameter must not equal value.
    Ne {
        /// Parameter name to compare.
        param: String,
        /// Value that parameter must not equal.
        value: Value,
    },

    // Set membership
    /// Parameter must be one of the values.
    In {
        /// Parameter name to check.
        param: String,
        /// Allowed values (parameter must match one of these).
        values: Vec<Value>,
    },
    /// Parameter must not be any of the values.
    NotIn {
        /// Parameter name to check.
        param: String,
        /// Forbidden values (parameter must not match any of these).
        values: Vec<Value>,
    },

    // String operations (no regex - containment is undecidable)
    /// Parameter must start with prefix.
    StartsWith {
        /// Parameter name to check (must be a string).
        param: String,
        /// Required prefix.
        prefix: String,
    },
    /// Parameter must end with suffix.
    EndsWith {
        /// Parameter name to check (must be a string).
        param: String,
        /// Required suffix.
        suffix: String,
    },
    /// Parameter must contain substring.
    Contains {
        /// Parameter name to check (must be a string).
        param: String,
        /// Required substring.
        substring: String,
    },

    // Existence (only constraints that don't require param to exist)
    /// Parameter must exist (have any value).
    Exists {
        /// Parameter name that must exist.
        param: String,
    },
    /// Parameter must not exist.
    NotExists {
        /// Parameter name that must not exist.
        param: String,
    },

    // Composite constraints
    /// All sub-constraints must pass (conjunction).
    And {
        /// Sub-constraints that must all pass.
        constraints: Vec<Constraint>,
    },
    /// At least one sub-constraint must pass (disjunction).
    Or {
        /// Sub-constraints where at least one must pass.
        constraints: Vec<Constraint>,
    },
}

impl Constraint {
    /// Evaluate this constraint against the given parameters.
    ///
    /// Returns `Ok(())` if the constraint passes, `Err` if it fails.
    #[allow(clippy::too_many_lines)]
    pub fn evaluate(&self, params: &Value) -> Result<(), ConstraintError> {
        match self {
            // Existence checks handle missing params specially
            Constraint::Exists { param } => {
                if get_param_opt(params, param).is_none() {
                    return Err(ConstraintError::MissingParam(param.clone()));
                }
                Ok(())
            }
            Constraint::NotExists { param } => {
                if let Some(val) = get_param_opt(params, param) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: "must not exist".to_string(),
                        actual: format!("{val:?}"),
                    });
                }
                Ok(())
            }

            // Comparison - fail-closed on missing param
            Constraint::Lt { param, value } => {
                let actual = get_param(params, param)?;
                if !compare_lt(&actual, value) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("< {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::Le { param, value } => {
                let actual = get_param(params, param)?;
                if !compare_le(&actual, value) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("<= {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::Gt { param, value } => {
                let actual = get_param(params, param)?;
                if !compare_gt(&actual, value) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("> {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::Ge { param, value } => {
                let actual = get_param(params, param)?;
                if !compare_ge(&actual, value) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!(">= {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::Eq { param, value } => {
                let actual = get_param(params, param)?;
                if &actual != value {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("== {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::Ne { param, value } => {
                let actual = get_param(params, param)?;
                if &actual == value {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("!= {value:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }

            // Set membership
            Constraint::In { param, values } => {
                let actual = get_param(params, param)?;
                if !values.contains(&actual) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("in {values:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }
            Constraint::NotIn { param, values } => {
                let actual = get_param(params, param)?;
                if values.contains(&actual) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("not in {values:?}"),
                        actual: format!("{actual:?}"),
                    });
                }
                Ok(())
            }

            // String operations
            Constraint::StartsWith { param, prefix } => {
                let actual = get_param_string(params, param)?;
                if !actual.starts_with(prefix) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("starts with \"{prefix}\""),
                        actual,
                    });
                }
                Ok(())
            }
            Constraint::EndsWith { param, suffix } => {
                let actual = get_param_string(params, param)?;
                if !actual.ends_with(suffix) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("ends with \"{suffix}\""),
                        actual,
                    });
                }
                Ok(())
            }
            Constraint::Contains { param, substring } => {
                let actual = get_param_string(params, param)?;
                if !actual.contains(substring) {
                    return Err(ConstraintError::Violation {
                        param: param.clone(),
                        rule: format!("contains \"{substring}\""),
                        actual,
                    });
                }
                Ok(())
            }

            // Composite constraints
            Constraint::And { constraints } => {
                for c in constraints {
                    c.evaluate(params)?;
                }
                Ok(())
            }
            Constraint::Or { constraints } => {
                if constraints.is_empty() {
                    return Err(ConstraintError::Violation {
                        param: "(or)".to_string(),
                        rule: "at least one constraint must match".to_string(),
                        actual: "empty Or".to_string(),
                    });
                }

                // Try each constraint, return Ok if any passes
                let mut last_error = None;
                for c in constraints {
                    match c.evaluate(params) {
                        Ok(()) => return Ok(()),
                        Err(e) => last_error = Some(e),
                    }
                }

                // All failed, return the last error
                Err(last_error.expect("Or has at least one constraint"))
            }
        }
    }

    /// Check if this constraint subsumes another (this >= other).
    ///
    /// Used for attenuation validation: parent must subsume child.
    /// Returns true if any value that satisfies `other` also satisfies `self`.
    pub fn subsumes(&self, other: &Constraint) -> bool {
        // Handle composite constraints specially
        match (self, other) {
            // And subsumes And if it subsumes all children
            (Constraint::And { constraints: p }, Constraint::And { constraints: c }) => {
                // Each parent constraint must be subsumed by the combined child constraints
                p.iter().all(|pc| c.iter().any(|cc| pc.subsumes(cc)))
            }
            // Or subsumes Or if each child has a matching parent alternative
            (Constraint::Or { constraints: p }, Constraint::Or { constraints: c }) => {
                // Each child alternative must be subsumed by some parent alternative
                c.iter().all(|cc| p.iter().any(|pc| pc.subsumes(cc)))
            }
            // Single constraint subsumes And if it subsumes any child
            (single, Constraint::And { constraints }) if single.param_name().is_some() => {
                constraints.iter().any(|c| single.subsumes(c))
            }
            // Single constraint subsumes Or if it subsumes all alternatives
            (single, Constraint::Or { constraints }) if single.param_name().is_some() => {
                constraints.iter().all(|c| single.subsumes(c))
            }
            // For non-composite, constraints must be on the same parameter
            _ => {
                let self_param = self.param_name();
                let other_param = other.param_name();

                // Both must have params and they must match
                if self_param.is_none() || other_param.is_none() {
                    return false;
                }
                if self_param != other_param {
                    return false;
                }

                self.subsumes_same_param(other)
            }
        }
    }

    /// Check subsumption for constraints on the same parameter.
    fn subsumes_same_param(&self, other: &Constraint) -> bool {
        match (self, other) {
            // Same constraint types
            (Constraint::Lt { value: p, .. }, Constraint::Lt { value: c, .. }) => {
                compare_le(c, p) // child.value <= parent.value
            }
            (
                Constraint::Le { value: p, .. },
                Constraint::Le { value: c, .. } | Constraint::Eq { value: c, .. },
            ) => compare_le(c, p),
            (Constraint::Gt { value: p, .. }, Constraint::Gt { value: c, .. }) => {
                compare_ge(c, p) // child.value >= parent.value
            }
            (
                Constraint::Ge { value: p, .. },
                Constraint::Ge { value: c, .. } | Constraint::Eq { value: c, .. },
            ) => compare_ge(c, p),
            (Constraint::Eq { value: p, .. }, Constraint::Eq { value: c, .. })
            | (Constraint::Ne { value: p, .. }, Constraint::Ne { value: c, .. }) => p == c,

            // Set membership
            (Constraint::In { values: p, .. }, Constraint::In { values: c, .. }) => {
                // Child values must be subset of parent values
                c.iter().all(|v| p.contains(v))
            }
            (Constraint::NotIn { values: p, .. }, Constraint::NotIn { values: c, .. }) => {
                // Child must exclude at least what parent excludes
                p.iter().all(|v| c.contains(v))
            }

            // String operations
            (
                Constraint::StartsWith { prefix: p, .. },
                Constraint::StartsWith { prefix: c, .. },
            ) => c.starts_with(p), // child prefix must extend parent
            (Constraint::EndsWith { suffix: p, .. }, Constraint::EndsWith { suffix: c, .. }) => {
                c.ends_with(p) // child suffix must extend parent
            }
            (
                Constraint::Contains { substring: p, .. },
                Constraint::Contains { substring: c, .. },
            ) => c.contains(p), // child must contain parent substring

            // Existence
            (Constraint::Exists { .. }, Constraint::Exists { .. })
            | (Constraint::NotExists { .. }, Constraint::NotExists { .. }) => true,

            // Cross-type subsumption: Eq subsumes all comparisons
            (Constraint::Eq { value, .. }, Constraint::Lt { value: c, .. }) => compare_lt(value, c),
            (Constraint::Eq { value, .. }, Constraint::Le { value: c, .. }) => compare_le(value, c),
            (Constraint::Eq { value, .. }, Constraint::Gt { value: c, .. }) => compare_gt(value, c),
            (Constraint::Eq { value, .. }, Constraint::Ge { value: c, .. }) => compare_ge(value, c),

            // Different constraint types on same param generally don't subsume
            _ => false,
        }
    }

    /// Get the parameter name this constraint applies to.
    ///
    /// Returns `None` for composite constraints (And, Or) which can span multiple params.
    pub fn param_name(&self) -> Option<&str> {
        match self {
            Constraint::Lt { param, .. }
            | Constraint::Le { param, .. }
            | Constraint::Gt { param, .. }
            | Constraint::Ge { param, .. }
            | Constraint::Eq { param, .. }
            | Constraint::Ne { param, .. }
            | Constraint::In { param, .. }
            | Constraint::NotIn { param, .. }
            | Constraint::StartsWith { param, .. }
            | Constraint::EndsWith { param, .. }
            | Constraint::Contains { param, .. }
            | Constraint::Exists { param }
            | Constraint::NotExists { param } => Some(param),
            // Composite constraints don't have a single param
            Constraint::And { .. } | Constraint::Or { .. } => None,
        }
    }

    /// Get all parameter names referenced by this constraint (including nested).
    pub fn referenced_params(&self) -> Vec<&str> {
        match self {
            Constraint::Lt { param, .. }
            | Constraint::Le { param, .. }
            | Constraint::Gt { param, .. }
            | Constraint::Ge { param, .. }
            | Constraint::Eq { param, .. }
            | Constraint::Ne { param, .. }
            | Constraint::In { param, .. }
            | Constraint::NotIn { param, .. }
            | Constraint::StartsWith { param, .. }
            | Constraint::EndsWith { param, .. }
            | Constraint::Contains { param, .. }
            | Constraint::Exists { param }
            | Constraint::NotExists { param } => vec![param],
            Constraint::And { constraints } | Constraint::Or { constraints } => constraints
                .iter()
                .flat_map(|c| c.referenced_params())
                .collect(),
        }
    }
}

/// A set of constraints with implicit AND semantics.
///
/// All constraints must pass for the set to pass.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ConstraintSet(pub Vec<Constraint>);

impl ConstraintSet {
    /// Create a new constraint set from a vector of constraints.
    pub fn new(constraints: Vec<Constraint>) -> Self {
        Self(constraints)
    }

    /// Create an empty constraint set.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Returns true if the constraint set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of constraints.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Get the constraints as a slice.
    pub fn constraints(&self) -> &[Constraint] {
        &self.0
    }

    /// Evaluate all constraints against parameters.
    ///
    /// Returns `Ok(())` if all constraints pass, or the first error encountered.
    pub fn evaluate(&self, params: &Value) -> Result<(), ConstraintError> {
        for constraint in &self.0 {
            constraint.evaluate(params)?;
        }
        Ok(())
    }

    /// Check if this constraint set subsumes another (for attenuation).
    ///
    /// Parent subsumes child if for every child constraint on a parameter
    /// that the parent also constrains, there exists a parent constraint
    /// that subsumes it. Child can add constraints on new parameters.
    pub fn subsumes(&self, child: &ConstraintSet) -> bool {
        for child_c in &child.0 {
            // Get params referenced by child constraint
            let child_params = child_c.referenced_params();

            // Check if parent constrains any of these params
            let parent_constrains_any = child_params
                .iter()
                .any(|&cp| self.0.iter().any(|pc| pc.referenced_params().contains(&cp)));

            if parent_constrains_any {
                // For composite constraints, check if parent subsumes the whole thing
                // For simple constraints, check param-by-param
                let parent_subsumes = if child_c.param_name().is_some() {
                    // Simple constraint - find matching parent constraint
                    self.0
                        .iter()
                        .any(|pc| pc.param_name() == child_c.param_name() && pc.subsumes(child_c))
                } else {
                    // Composite constraint - check if any parent subsumes it
                    self.0.iter().any(|pc| pc.subsumes(child_c))
                };

                if !parent_subsumes {
                    return false;
                }
            }
            // If parent doesn't constrain these params, child can add constraint
        }
        true
    }

    /// Merge another constraint set into this one (conjunction).
    ///
    /// Used for attenuation: new constraints are AND-ed with existing ones.
    pub fn extend(&mut self, other: ConstraintSet) {
        self.0.extend(other.0);
    }

    /// Create a new constraint set by merging two sets.
    #[must_use]
    pub fn merge(self, other: ConstraintSet) -> Self {
        let mut result = self;
        result.extend(other);
        result
    }
}

// Helper functions

fn get_param(params: &Value, path: &str) -> Result<Value, ConstraintError> {
    get_param_opt(params, path).ok_or_else(|| ConstraintError::MissingParam(path.to_string()))
}

fn get_param_opt(params: &Value, path: &str) -> Option<Value> {
    // Support both "foo" and "/foo" style paths
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    params.pointer(&path).cloned()
}

fn get_param_string(params: &Value, path: &str) -> Result<String, ConstraintError> {
    let val = get_param(params, path)?;
    val.as_str()
        .map(std::string::ToString::to_string)
        .ok_or_else(|| ConstraintError::TypeMismatch {
            param: path.to_string(),
            expected: "string".to_string(),
            actual: format!("{val:?}"),
        })
}

fn compare_lt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            a.as_f64().unwrap_or(f64::NAN) < b.as_f64().unwrap_or(f64::NAN)
        }
        (Value::String(a), Value::String(b)) => a < b,
        _ => false,
    }
}

fn compare_le(a: &Value, b: &Value) -> bool {
    a == b || compare_lt(a, b)
}

fn compare_gt(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(a), Value::Number(b)) => {
            a.as_f64().unwrap_or(f64::NAN) > b.as_f64().unwrap_or(f64::NAN)
        }
        (Value::String(a), Value::String(b)) => a > b,
        _ => false,
    }
}

fn compare_ge(a: &Value, b: &Value) -> bool {
    a == b || compare_gt(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_le_constraint() {
        let constraint = Constraint::Le {
            param: "amount".to_string(),
            value: json!(100),
        };

        // Pass
        assert!(constraint.evaluate(&json!({"amount": 50})).is_ok());
        assert!(constraint.evaluate(&json!({"amount": 100})).is_ok());

        // Fail
        assert!(constraint.evaluate(&json!({"amount": 101})).is_err());
        assert!(constraint.evaluate(&json!({})).is_err()); // missing
    }

    #[test]
    fn test_in_constraint() {
        let constraint = Constraint::In {
            param: "currency".to_string(),
            values: vec![json!("USD"), json!("EUR")],
        };

        assert!(constraint.evaluate(&json!({"currency": "USD"})).is_ok());
        assert!(constraint.evaluate(&json!({"currency": "EUR"})).is_ok());
        assert!(constraint.evaluate(&json!({"currency": "GBP"})).is_err());
    }

    #[test]
    fn test_starts_with_constraint() {
        let constraint = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/api/v2/".to_string(),
        };

        assert!(
            constraint
                .evaluate(&json!({"path": "/api/v2/users"}))
                .is_ok()
        );
        assert!(
            constraint
                .evaluate(&json!({"path": "/api/v1/users"}))
                .is_err()
        );
    }

    #[test]
    fn test_exists_constraint() {
        let constraint = Constraint::Exists {
            param: "customer_id".to_string(),
        };

        assert!(
            constraint
                .evaluate(&json!({"customer_id": "cus_123"}))
                .is_ok()
        );
        assert!(constraint.evaluate(&json!({})).is_err());
    }

    #[test]
    fn test_constraint_set() {
        let set = ConstraintSet::new(vec![
            Constraint::Ge {
                param: "amount".into(),
                value: json!(100),
            },
            Constraint::Le {
                param: "amount".into(),
                value: json!(10000),
            },
        ]);

        assert!(set.evaluate(&json!({"amount": 500})).is_ok());
        assert!(set.evaluate(&json!({"amount": 50})).is_err());
        assert!(set.evaluate(&json!({"amount": 50000})).is_err());
    }

    #[test]
    fn test_subsumes() {
        // Le <= Le
        let parent = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };
        let child = Constraint::Le {
            param: "x".to_string(),
            value: json!(50),
        };
        assert!(parent.subsumes(&child));
        assert!(!child.subsumes(&parent));

        // In subsumes subset
        let parent = Constraint::In {
            param: "x".to_string(),
            values: vec![json!("a"), json!("b"), json!("c")],
        };
        let child = Constraint::In {
            param: "x".to_string(),
            values: vec![json!("a"), json!("b")],
        };
        assert!(parent.subsumes(&child));
        assert!(!child.subsumes(&parent));
    }

    #[test]
    fn test_constraint_set_subsumes() {
        let parent = ConstraintSet::new(vec![Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        }]);

        // Child can make constraint stricter
        let child = ConstraintSet::new(vec![Constraint::Le {
            param: "x".to_string(),
            value: json!(50),
        }]);
        assert!(parent.subsumes(&child));

        // Child cannot make constraint looser
        let child = ConstraintSet::new(vec![Constraint::Le {
            param: "x".to_string(),
            value: json!(200),
        }]);
        assert!(!parent.subsumes(&child));

        // Child can add new constraints
        let child = ConstraintSet::new(vec![
            Constraint::Le {
                param: "x".to_string(),
                value: json!(50),
            },
            Constraint::Ge {
                param: "y".to_string(),
                value: json!(0),
            },
        ]);
        assert!(parent.subsumes(&child));
    }

    #[test]
    fn test_or_constraint() {
        // currency == "USD" OR currency == "EUR"
        let constraint = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "currency".to_string(),
                    value: json!("USD"),
                },
                Constraint::Eq {
                    param: "currency".to_string(),
                    value: json!("EUR"),
                },
            ],
        };

        assert!(constraint.evaluate(&json!({"currency": "USD"})).is_ok());
        assert!(constraint.evaluate(&json!({"currency": "EUR"})).is_ok());
        assert!(constraint.evaluate(&json!({"currency": "GBP"})).is_err());
    }

    #[test]
    fn test_and_constraint() {
        // amount >= 100 AND amount <= 1000
        let constraint = Constraint::And {
            constraints: vec![
                Constraint::Ge {
                    param: "amount".to_string(),
                    value: json!(100),
                },
                Constraint::Le {
                    param: "amount".to_string(),
                    value: json!(1000),
                },
            ],
        };

        assert!(constraint.evaluate(&json!({"amount": 500})).is_ok());
        assert!(constraint.evaluate(&json!({"amount": 100})).is_ok());
        assert!(constraint.evaluate(&json!({"amount": 1000})).is_ok());
        assert!(constraint.evaluate(&json!({"amount": 50})).is_err());
        assert!(constraint.evaluate(&json!({"amount": 1500})).is_err());
    }

    #[test]
    fn test_nested_or_and() {
        // (type == "refund" AND amount <= 100) OR type == "charge"
        let constraint = Constraint::Or {
            constraints: vec![
                Constraint::And {
                    constraints: vec![
                        Constraint::Eq {
                            param: "type".to_string(),
                            value: json!("refund"),
                        },
                        Constraint::Le {
                            param: "amount".to_string(),
                            value: json!(100),
                        },
                    ],
                },
                Constraint::Eq {
                    param: "type".to_string(),
                    value: json!("charge"),
                },
            ],
        };

        // Charge with any amount passes
        assert!(
            constraint
                .evaluate(&json!({"type": "charge", "amount": 9999}))
                .is_ok()
        );

        // Refund with small amount passes
        assert!(
            constraint
                .evaluate(&json!({"type": "refund", "amount": 50}))
                .is_ok()
        );

        // Refund with large amount fails
        assert!(
            constraint
                .evaluate(&json!({"type": "refund", "amount": 500}))
                .is_err()
        );
    }

    #[test]
    fn test_referenced_params() {
        let simple = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };
        assert_eq!(simple.referenced_params(), vec!["x"]);

        let composite = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "a".to_string(),
                    value: json!(1),
                },
                Constraint::Eq {
                    param: "b".to_string(),
                    value: json!(2),
                },
            ],
        };
        let params = composite.referenced_params();
        assert!(params.contains(&"a"));
        assert!(params.contains(&"b"));
    }

    #[test]
    fn test_constraint_set_merge() {
        let set1 = ConstraintSet::new(vec![Constraint::Ge {
            param: "x".to_string(),
            value: json!(0),
        }]);
        let set2 = ConstraintSet::new(vec![Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        }]);

        let merged = set1.merge(set2);
        assert_eq!(merged.len(), 2);

        // Both constraints apply
        assert!(merged.evaluate(&json!({"x": 50})).is_ok());
        assert!(merged.evaluate(&json!({"x": -1})).is_err());
        assert!(merged.evaluate(&json!({"x": 101})).is_err());
    }

    // === Additional tests for uncovered constraint types ===

    #[test]
    fn test_not_exists_constraint() {
        let constraint = Constraint::NotExists {
            param: "deprecated".to_string(),
        };

        // Pass when param is absent
        assert!(constraint.evaluate(&json!({})).is_ok());
        assert!(constraint.evaluate(&json!({"other": "value"})).is_ok());

        // Fail when param exists
        let result = constraint.evaluate(&json!({"deprecated": "yes"}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "deprecated");
                assert!(rule.contains("must not exist"));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_lt_constraint() {
        let constraint = Constraint::Lt {
            param: "x".to_string(),
            value: json!(100),
        };

        // Pass
        assert!(constraint.evaluate(&json!({"x": 50})).is_ok());
        assert!(constraint.evaluate(&json!({"x": 99})).is_ok());

        // Fail - equal is not less than
        let result = constraint.evaluate(&json!({"x": 100}));
        assert!(result.is_err());

        // Fail - greater than
        let result = constraint.evaluate(&json!({"x": 101}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "x");
                assert!(rule.contains("< "));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_gt_constraint() {
        let constraint = Constraint::Gt {
            param: "x".to_string(),
            value: json!(0),
        };

        // Pass
        assert!(constraint.evaluate(&json!({"x": 1})).is_ok());
        assert!(constraint.evaluate(&json!({"x": 100})).is_ok());

        // Fail - equal is not greater than
        let result = constraint.evaluate(&json!({"x": 0}));
        assert!(result.is_err());

        // Fail - less than
        let result = constraint.evaluate(&json!({"x": -1}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "x");
                assert!(rule.contains("> "));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_ne_constraint() {
        let constraint = Constraint::Ne {
            param: "status".to_string(),
            value: json!("deleted"),
        };

        // Pass
        assert!(constraint.evaluate(&json!({"status": "active"})).is_ok());
        assert!(constraint.evaluate(&json!({"status": "pending"})).is_ok());

        // Fail - equal to forbidden value
        let result = constraint.evaluate(&json!({"status": "deleted"}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "status");
                assert!(rule.contains("!= "));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_not_in_constraint() {
        let constraint = Constraint::NotIn {
            param: "method".to_string(),
            values: vec![json!("DELETE"), json!("DROP")],
        };

        // Pass
        assert!(constraint.evaluate(&json!({"method": "SELECT"})).is_ok());
        assert!(constraint.evaluate(&json!({"method": "INSERT"})).is_ok());

        // Fail - value is in forbidden set
        let result = constraint.evaluate(&json!({"method": "DELETE"}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "method");
                assert!(rule.contains("not in"));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_ends_with_constraint() {
        let constraint = Constraint::EndsWith {
            param: "file".to_string(),
            suffix: ".json".to_string(),
        };

        // Pass
        assert!(constraint.evaluate(&json!({"file": "data.json"})).is_ok());
        assert!(
            constraint
                .evaluate(&json!({"file": "config/settings.json"}))
                .is_ok()
        );

        // Fail
        let result = constraint.evaluate(&json!({"file": "data.xml"}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "file");
                assert!(rule.contains("ends with"));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_contains_constraint() {
        let constraint = Constraint::Contains {
            param: "query".to_string(),
            substring: "SELECT".to_string(),
        };

        // Pass
        assert!(
            constraint
                .evaluate(&json!({"query": "SELECT * FROM users"}))
                .is_ok()
        );
        assert!(
            constraint
                .evaluate(&json!({"query": "Running SELECT query"}))
                .is_ok()
        );

        // Fail
        let result = constraint.evaluate(&json!({"query": "DELETE FROM users"}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "query");
                assert!(rule.contains("contains"));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_empty_or_constraint() {
        let constraint = Constraint::Or {
            constraints: vec![],
        };

        // Empty Or always fails
        let result = constraint.evaluate(&json!({"x": 1}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::Violation { param, rule, .. }) => {
                assert_eq!(param, "(or)");
                assert!(rule.contains("at least one"));
            }
            _ => panic!("Expected Violation error"),
        }
    }

    #[test]
    fn test_constraint_set_empty_and_accessors() {
        let empty = ConstraintSet::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert!(empty.constraints().is_empty());

        // Empty set passes all params
        assert!(empty.evaluate(&json!({"any": "value"})).is_ok());

        let non_empty = ConstraintSet::new(vec![Constraint::Exists {
            param: "x".to_string(),
        }]);
        assert!(!non_empty.is_empty());
        assert_eq!(non_empty.len(), 1);
        assert_eq!(non_empty.constraints().len(), 1);
    }

    // === Subsumption tests ===

    #[test]
    fn test_lt_subsumes_lt() {
        let parent = Constraint::Lt {
            param: "x".to_string(),
            value: json!(100),
        };
        let child = Constraint::Lt {
            param: "x".to_string(),
            value: json!(50),
        };
        let looser = Constraint::Lt {
            param: "x".to_string(),
            value: json!(150),
        };

        assert!(parent.subsumes(&child)); // 50 < 100, stricter
        assert!(!parent.subsumes(&looser)); // 150 > 100, looser
    }

    #[test]
    fn test_gt_subsumes_gt() {
        let parent = Constraint::Gt {
            param: "x".to_string(),
            value: json!(0),
        };
        let child = Constraint::Gt {
            param: "x".to_string(),
            value: json!(10),
        };
        let looser = Constraint::Gt {
            param: "x".to_string(),
            value: json!(-10),
        };

        assert!(parent.subsumes(&child)); // > 10 is stricter than > 0
        assert!(!parent.subsumes(&looser)); // > -10 is looser than > 0
    }

    #[test]
    fn test_eq_ne_subsumes() {
        let eq1 = Constraint::Eq {
            param: "x".to_string(),
            value: json!(100),
        };
        let eq2 = Constraint::Eq {
            param: "x".to_string(),
            value: json!(100),
        };
        let eq3 = Constraint::Eq {
            param: "x".to_string(),
            value: json!(50),
        };

        assert!(eq1.subsumes(&eq2)); // Same value
        assert!(!eq1.subsumes(&eq3)); // Different value

        let ne1 = Constraint::Ne {
            param: "x".to_string(),
            value: json!("a"),
        };
        let ne2 = Constraint::Ne {
            param: "x".to_string(),
            value: json!("a"),
        };
        let ne3 = Constraint::Ne {
            param: "x".to_string(),
            value: json!("b"),
        };

        assert!(ne1.subsumes(&ne2));
        assert!(!ne1.subsumes(&ne3));
    }

    #[test]
    fn test_not_in_subsumes_not_in() {
        let parent = Constraint::NotIn {
            param: "x".to_string(),
            values: vec![json!("a"), json!("b")],
        };
        let child = Constraint::NotIn {
            param: "x".to_string(),
            values: vec![json!("a"), json!("b"), json!("c")],
        };
        let looser = Constraint::NotIn {
            param: "x".to_string(),
            values: vec![json!("a")],
        };

        assert!(parent.subsumes(&child)); // Child excludes more
        assert!(!parent.subsumes(&looser)); // Looser excludes less
    }

    #[test]
    fn test_starts_with_subsumes() {
        let parent = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/api".to_string(),
        };
        let child = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/api/v2".to_string(),
        };
        let looser = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/ap".to_string(),
        };

        assert!(parent.subsumes(&child)); // Child prefix extends parent
        assert!(!parent.subsumes(&looser)); // Looser prefix is shorter
    }

    #[test]
    fn test_ends_with_subsumes() {
        let parent = Constraint::EndsWith {
            param: "file".to_string(),
            suffix: ".json".to_string(),
        };
        let child = Constraint::EndsWith {
            param: "file".to_string(),
            suffix: "config.json".to_string(),
        };
        let looser = Constraint::EndsWith {
            param: "file".to_string(),
            suffix: "son".to_string(),
        };

        assert!(parent.subsumes(&child)); // Child suffix extends parent
        assert!(!parent.subsumes(&looser)); // Looser suffix doesn't end with .json
    }

    #[test]
    fn test_contains_subsumes() {
        let parent = Constraint::Contains {
            param: "query".to_string(),
            substring: "SELECT".to_string(),
        };
        let child = Constraint::Contains {
            param: "query".to_string(),
            substring: "SELECT *".to_string(),
        };

        assert!(parent.subsumes(&child)); // Child substring contains parent
    }

    #[test]
    fn test_exists_not_exists_subsumes() {
        let exists1 = Constraint::Exists {
            param: "x".to_string(),
        };
        let exists2 = Constraint::Exists {
            param: "x".to_string(),
        };

        assert!(exists1.subsumes(&exists2));

        let not_exists1 = Constraint::NotExists {
            param: "x".to_string(),
        };
        let not_exists2 = Constraint::NotExists {
            param: "x".to_string(),
        };

        assert!(not_exists1.subsumes(&not_exists2));
    }

    #[test]
    fn test_eq_subsumes_comparisons() {
        let eq = Constraint::Eq {
            param: "x".to_string(),
            value: json!(50),
        };

        // Eq(50) subsumes Lt(100) because 50 < 100
        let lt = Constraint::Lt {
            param: "x".to_string(),
            value: json!(100),
        };
        assert!(eq.subsumes(&lt));

        // Eq(50) subsumes Le(50) because 50 <= 50
        let le = Constraint::Le {
            param: "x".to_string(),
            value: json!(50),
        };
        assert!(eq.subsumes(&le));

        // Eq(50) subsumes Gt(0) because 50 > 0
        let gt = Constraint::Gt {
            param: "x".to_string(),
            value: json!(0),
        };
        assert!(eq.subsumes(&gt));

        // Eq(50) subsumes Ge(50) because 50 >= 50
        let ge = Constraint::Ge {
            param: "x".to_string(),
            value: json!(50),
        };
        assert!(eq.subsumes(&ge));
    }

    #[test]
    fn test_le_subsumes_eq() {
        let le = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };
        let eq = Constraint::Eq {
            param: "x".to_string(),
            value: json!(50),
        };

        // Le(100) subsumes Eq(50) because 50 <= 100
        assert!(le.subsumes(&eq));
    }

    #[test]
    fn test_ge_subsumes_eq() {
        let ge = Constraint::Ge {
            param: "x".to_string(),
            value: json!(0),
        };
        let eq = Constraint::Eq {
            param: "x".to_string(),
            value: json!(50),
        };

        // Ge(0) subsumes Eq(50) because 50 >= 0
        assert!(ge.subsumes(&eq));
    }

    #[test]
    fn test_different_params_no_subsumption() {
        let c1 = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };
        let c2 = Constraint::Le {
            param: "y".to_string(),
            value: json!(50),
        };

        // Different params don't subsume each other
        assert!(!c1.subsumes(&c2));
    }

    #[test]
    fn test_and_subsumes_and() {
        let parent = Constraint::And {
            constraints: vec![Constraint::Le {
                param: "x".to_string(),
                value: json!(100),
            }],
        };

        let child = Constraint::And {
            constraints: vec![
                Constraint::Le {
                    param: "x".to_string(),
                    value: json!(50),
                },
                Constraint::Ge {
                    param: "x".to_string(),
                    value: json!(0),
                },
            ],
        };

        // Parent And subsumes child And if parent subsumes any child constraint
        assert!(parent.subsumes(&child));
    }

    #[test]
    fn test_or_subsumes_or() {
        let parent = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!("a"),
                },
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!("b"),
                },
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!("c"),
                },
            ],
        };

        let child = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!("a"),
                },
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!("b"),
                },
            ],
        };

        // Parent Or subsumes child Or if each child alt is subsumed by some parent alt
        assert!(parent.subsumes(&child));
    }

    #[test]
    fn test_single_subsumes_and() {
        let single = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };

        let and_constraint = Constraint::And {
            constraints: vec![
                Constraint::Le {
                    param: "x".to_string(),
                    value: json!(50),
                },
                Constraint::Ge {
                    param: "y".to_string(),
                    value: json!(0),
                },
            ],
        };

        // Single subsumes And if it subsumes any child
        assert!(single.subsumes(&and_constraint));
    }

    #[test]
    fn test_single_subsumes_or() {
        let single = Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        };

        let or_constraint = Constraint::Or {
            constraints: vec![
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!(50),
                },
                Constraint::Eq {
                    param: "x".to_string(),
                    value: json!(75),
                },
            ],
        };

        // Single subsumes Or if it subsumes ALL alternatives
        assert!(single.subsumes(&or_constraint));
    }

    #[test]
    fn test_constraint_set_subsumes_composite() {
        let parent = ConstraintSet::new(vec![Constraint::Le {
            param: "x".to_string(),
            value: json!(100),
        }]);

        let child = ConstraintSet::new(vec![Constraint::And {
            constraints: vec![
                Constraint::Le {
                    param: "x".to_string(),
                    value: json!(50),
                },
                Constraint::Ge {
                    param: "x".to_string(),
                    value: json!(0),
                },
            ],
        }]);

        // Parent set subsumes child with composite constraint
        assert!(parent.subsumes(&child));
    }

    #[test]
    fn test_param_name_for_composite() {
        let and_c = Constraint::And {
            constraints: vec![],
        };
        let or_c = Constraint::Or {
            constraints: vec![],
        };

        assert!(and_c.param_name().is_none());
        assert!(or_c.param_name().is_none());
    }

    #[test]
    fn test_string_comparisons() {
        // Lt with strings
        let lt_str = Constraint::Lt {
            param: "name".to_string(),
            value: json!("m"),
        };
        assert!(lt_str.evaluate(&json!({"name": "apple"})).is_ok()); // "apple" < "m"
        assert!(lt_str.evaluate(&json!({"name": "zebra"})).is_err()); // "zebra" > "m"

        // Gt with strings
        let gt_str = Constraint::Gt {
            param: "name".to_string(),
            value: json!("m"),
        };
        assert!(gt_str.evaluate(&json!({"name": "zebra"})).is_ok()); // "zebra" > "m"
        assert!(gt_str.evaluate(&json!({"name": "apple"})).is_err()); // "apple" < "m"
    }

    #[test]
    fn test_type_mismatch_error() {
        let constraint = Constraint::StartsWith {
            param: "path".to_string(),
            prefix: "/api/".to_string(),
        };

        // String constraint on non-string value
        let result = constraint.evaluate(&json!({"path": 123}));
        assert!(result.is_err());
        match result {
            Err(ConstraintError::TypeMismatch {
                param, expected, ..
            }) => {
                assert_eq!(param, "path");
                assert_eq!(expected, "string");
            }
            _ => panic!("Expected TypeMismatch error"),
        }
    }

    #[test]
    fn test_json_path_style() {
        // Both "/foo" and "foo" style paths should work
        let constraint = Constraint::Eq {
            param: "/nested/key".to_string(),
            value: json!("value"),
        };

        assert!(
            constraint
                .evaluate(&json!({"nested": {"key": "value"}}))
                .is_ok()
        );
    }

    #[test]
    fn test_default_constraint_set() {
        let default_set: ConstraintSet = ConstraintSet::default();
        assert!(default_set.is_empty());
    }
}

/// Property-based tests for constraint subsumption
#[cfg(all(test, not(target_arch = "wasm32")))]
mod proptests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    // Strategy for generating constraint bounds
    fn bound_strategy() -> impl Strategy<Value = i64> {
        -1000i64..1000i64
    }

    // Strategy for generating test values
    fn value_strategy() -> impl Strategy<Value = i64> {
        -2000i64..2000i64
    }

    proptest! {
        /// If Le(parent_bound) subsumes Le(child_bound), then child_bound <= parent_bound
        #[test]
        fn le_subsumption_means_child_stricter(
            parent_bound in bound_strategy(),
            child_bound in bound_strategy()
        ) {
            let parent = Constraint::Le {
                param: "x".to_string(),
                value: json!(parent_bound),
            };
            let child = Constraint::Le {
                param: "x".to_string(),
                value: json!(child_bound),
            };

            let subsumes = parent.subsumes(&child);
            if subsumes {
                prop_assert!(child_bound <= parent_bound,
                    "parent Le({}) subsumes child Le({}) but {} > {}",
                    parent_bound, child_bound, child_bound, parent_bound);
            }
        }

        /// If Ge(parent_bound) subsumes Ge(child_bound), then child_bound >= parent_bound
        #[test]
        fn ge_subsumption_means_child_stricter(
            parent_bound in bound_strategy(),
            child_bound in bound_strategy()
        ) {
            let parent = Constraint::Ge {
                param: "x".to_string(),
                value: json!(parent_bound),
            };
            let child = Constraint::Ge {
                param: "x".to_string(),
                value: json!(child_bound),
            };

            let subsumes = parent.subsumes(&child);
            if subsumes {
                prop_assert!(child_bound >= parent_bound,
                    "parent Ge({}) subsumes child Ge({}) but {} < {}",
                    parent_bound, child_bound, child_bound, parent_bound);
            }
        }

        /// If parent subsumes child, any value passing child also passes parent
        #[test]
        fn subsumption_implies_pass_through(
            parent_bound in bound_strategy(),
            child_bound in bound_strategy(),
            test_value in value_strategy()
        ) {
            let parent = Constraint::Le {
                param: "x".to_string(),
                value: json!(parent_bound),
            };
            let child = Constraint::Le {
                param: "x".to_string(),
                value: json!(child_bound),
            };

            if parent.subsumes(&child) {
                let params = json!({"x": test_value});
                let child_passes = child.evaluate(&params).is_ok();
                let parent_passes = parent.evaluate(&params).is_ok();

                // If child passes, parent must also pass
                if child_passes {
                    prop_assert!(parent_passes,
                        "child Le({}) passes for x={} but parent Le({}) fails",
                        child_bound, test_value, parent_bound);
                }
            }
        }

        /// Constraint evaluation is deterministic
        #[test]
        fn evaluation_is_deterministic(
            bound in bound_strategy(),
            test_value in value_strategy()
        ) {
            let constraint = Constraint::Le {
                param: "x".to_string(),
                value: json!(bound),
            };
            let params = json!({"x": test_value});

            let result1 = constraint.evaluate(&params);
            let result2 = constraint.evaluate(&params);

            prop_assert_eq!(result1.is_ok(), result2.is_ok(),
                "constraint evaluation not deterministic for Le({}) with x={}",
                bound, test_value);
        }

        /// ConstraintSet subsumption: if parent subsumes child, child values pass parent
        #[test]
        fn constraint_set_subsumption(
            parent_min in bound_strategy(),
            parent_max in bound_strategy(),
            child_min in bound_strategy(),
            child_max in bound_strategy(),
            test_value in value_strategy()
        ) {
            // Ensure valid ranges (min <= max)
            let parent_min = parent_min.min(parent_max);
            let parent_max = parent_min.max(parent_max);
            let child_min = child_min.min(child_max);
            let child_max = child_min.max(child_max);

            let parent = ConstraintSet::new(vec![
                Constraint::Ge { param: "x".to_string(), value: json!(parent_min) },
                Constraint::Le { param: "x".to_string(), value: json!(parent_max) },
            ]);
            let child = ConstraintSet::new(vec![
                Constraint::Ge { param: "x".to_string(), value: json!(child_min) },
                Constraint::Le { param: "x".to_string(), value: json!(child_max) },
            ]);

            if parent.subsumes(&child) {
                let params = json!({"x": test_value});
                let child_passes = child.evaluate(&params).is_ok();
                let parent_passes = parent.evaluate(&params).is_ok();

                if child_passes {
                    prop_assert!(parent_passes,
                        "child range [{}, {}] passes x={} but parent [{}, {}] fails",
                        child_min, child_max, test_value, parent_min, parent_max);
                }
            }
        }
    }
}
