//! Transition validators for capability types.
//!
//! This module provides [`TransitionValidator`] implementations for the
//! capability types in this crate.

use amla_protocol::{CapabilityData, TransitionError, TransitionValidator};

use crate::method::{METHOD_CAPABILITY_TYPE, MethodCapability};

/// Validator for "method" capability type.
///
/// Validates that child method capabilities are valid attenuations of parents:
/// 1. Child pattern is a subset of parent pattern
/// 2. Child constraints are at least as restrictive as parent
/// 3. Child `max_calls` ≤ parent `max_calls` (if parent has limit)
///
/// # Example
///
/// ```rust
/// use amla_capabilities::method::MethodCapability;
/// use amla_capabilities::validator::MethodValidator;
/// use amla_protocol::TransitionValidator;
///
/// let validator = MethodValidator;
///
/// let parent = MethodCapability::new("stripe/**")
///     .with_max_calls(100);
/// let parent_data = parent.to_capability_data().unwrap();
///
/// let child = MethodCapability::new("stripe/charges/*")
///     .with_max_calls(50);
/// let child_data = child.to_capability_data().unwrap();
///
/// assert!(validator.validate_transition(&parent_data, &child_data).is_ok());
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct MethodValidator;

impl TransitionValidator for MethodValidator {
    fn validate_transition(
        &self,
        parent: &CapabilityData,
        child: &CapabilityData,
    ) -> Result<(), TransitionError> {
        // Check type matches
        if parent.capability_type() != METHOD_CAPABILITY_TYPE {
            return Err(TransitionError::new(format!(
                "expected parent type '{}', got '{}'",
                METHOD_CAPABILITY_TYPE,
                parent.capability_type()
            )));
        }

        if child.capability_type() != METHOD_CAPABILITY_TYPE {
            return Err(TransitionError::new(format!(
                "expected child type '{}', got '{}'",
                METHOD_CAPABILITY_TYPE,
                child.capability_type()
            )));
        }

        // Decode capabilities
        let parent_cap = MethodCapability::from_capability_data(parent)
            .map_err(|e| TransitionError::new(format!("decode parent: {e}")))?;
        let child_cap = MethodCapability::from_capability_data(child)
            .map_err(|e| TransitionError::new(format!("decode child: {e}")))?;

        // Check attenuation
        if !child_cap.is_subset_of(&parent_cap) {
            return Err(TransitionError::new(
                "child is not a valid attenuation of parent",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amla_constraints::{Constraint, ConstraintSet};
    use serde_json::json;

    fn make_method_data(pattern: &str) -> CapabilityData {
        MethodCapability::new(pattern).to_capability_data().unwrap()
    }

    fn make_method_data_with_limit(pattern: &str, max_calls: u32) -> CapabilityData {
        MethodCapability::new(pattern)
            .with_max_calls(max_calls)
            .to_capability_data()
            .unwrap()
    }

    fn make_method_data_with_constraints(
        pattern: &str,
        constraints: ConstraintSet,
    ) -> CapabilityData {
        MethodCapability::new(pattern)
            .with_constraints(constraints)
            .to_capability_data()
            .unwrap()
    }

    #[test]
    fn test_valid_pattern_attenuation() {
        let validator = MethodValidator;

        let parent = make_method_data("stripe/**");
        let child = make_method_data("stripe/charges/*");

        assert!(validator.validate_transition(&parent, &child).is_ok());
    }

    #[test]
    fn test_invalid_pattern_escalation() {
        let validator = MethodValidator;

        let parent = make_method_data("stripe/charges/*");
        let child = make_method_data("stripe/**"); // Broader!

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .reason
                .contains("not a valid attenuation")
        );
    }

    #[test]
    fn test_valid_max_calls_attenuation() {
        let validator = MethodValidator;

        let parent = make_method_data_with_limit("stripe/**", 100);
        let child = make_method_data_with_limit("stripe/**", 50);

        assert!(validator.validate_transition(&parent, &child).is_ok());
    }

    #[test]
    fn test_invalid_max_calls_escalation() {
        let validator = MethodValidator;

        let parent = make_method_data_with_limit("stripe/**", 100);
        let child = make_method_data_with_limit("stripe/**", 200); // Higher!

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_unlimited_when_parent_limited() {
        let validator = MethodValidator;

        let parent = make_method_data_with_limit("stripe/**", 100);
        let child = make_method_data("stripe/**"); // No limit!

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
    }

    #[test]
    fn test_valid_constraint_attenuation() {
        let validator = MethodValidator;

        let parent = make_method_data_with_constraints(
            "stripe/**",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            }]),
        );

        let child = make_method_data_with_constraints(
            "stripe/charges/*",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(5000), // Stricter
            }]),
        );

        assert!(validator.validate_transition(&parent, &child).is_ok());
    }

    #[test]
    fn test_invalid_constraint_escalation() {
        let validator = MethodValidator;

        let parent = make_method_data_with_constraints(
            "stripe/**",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            }]),
        );

        let child = make_method_data_with_constraints(
            "stripe/charges/*",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(20000), // Looser!
            }]),
        );

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
    }

    #[test]
    fn test_wrong_parent_type() {
        let validator = MethodValidator;

        let parent =
            CapabilityData::new("cap:test", "wrong-type", &json!({"method_pattern": "test"}))
                .unwrap();
        let child = make_method_data("stripe/**");

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
        assert!(result.unwrap_err().reason.contains("expected parent type"));
    }

    #[test]
    fn test_wrong_child_type() {
        let validator = MethodValidator;

        let parent = make_method_data("stripe/**");
        let child =
            CapabilityData::new("cap:test", "wrong-type", &json!({"method_pattern": "test"}))
                .unwrap();

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
        assert!(result.unwrap_err().reason.contains("expected child type"));
    }

    #[test]
    fn test_same_capability_is_valid() {
        let validator = MethodValidator;

        let cap = make_method_data("stripe/charges/*");

        assert!(validator.validate_transition(&cap, &cap).is_ok());
    }

    #[test]
    fn test_disjoint_patterns_invalid() {
        let validator = MethodValidator;

        let parent = make_method_data("stripe/**");
        let child = make_method_data("github/**"); // Different prefix!

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
    }
}
