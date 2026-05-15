//! WASM exports for method capabilities.
//!
//! This module provides JavaScript-callable functions for:
//! - Validating method calls against capabilities
//! - Checking capability transitions (attenuation)
//! - Pattern matching utilities

use wasm_bindgen::prelude::*;

use crate::method::MethodCapability;
use crate::patterns;

/// Validate a method call against a capability.
///
/// # Arguments
///
/// * `capability_json` - JSON-serialized `MethodCapability`
/// * `method` - The method name being called
/// * `params_json` - JSON-serialized parameters
///
/// # Returns
///
/// Returns `Ok(())` if the call is valid, or an error message.
#[wasm_bindgen]
pub fn validate_method_call(
    capability_json: &str,
    method: &str,
    params_json: &str,
) -> Result<(), JsValue> {
    let cap: MethodCapability = serde_json::from_str(capability_json)
        .map_err(|e| JsValue::from_str(&format!("invalid capability JSON: {e}")))?;

    let params: serde_json::Value = serde_json::from_str(params_json)
        .map_err(|e| JsValue::from_str(&format!("invalid params JSON: {e}")))?;

    cap.validate_call(method, &params)
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Check if a child capability is a valid attenuation of a parent.
///
/// # Arguments
///
/// * `parent_json` - JSON-serialized parent `MethodCapability`
/// * `child_json` - JSON-serialized child `MethodCapability`
///
/// # Returns
///
/// Returns `Ok(())` if the transition is valid, or an error message.
#[wasm_bindgen]
pub fn validate_method_transition(parent_json: &str, child_json: &str) -> Result<(), JsValue> {
    let parent: MethodCapability = serde_json::from_str(parent_json)
        .map_err(|e| JsValue::from_str(&format!("invalid parent JSON: {e}")))?;

    let child: MethodCapability = serde_json::from_str(child_json)
        .map_err(|e| JsValue::from_str(&format!("invalid child JSON: {e}")))?;

    if child.is_subset_of(&parent) {
        Ok(())
    } else {
        Err(JsValue::from_str(
            "child is not a valid attenuation of parent",
        ))
    }
}

/// Check if a child capability is a subset of a parent.
///
/// # Arguments
///
/// * `child_json` - JSON-serialized child `MethodCapability`
/// * `parent_json` - JSON-serialized parent `MethodCapability`
///
/// # Returns
///
/// Returns `true` if child is a subset of parent.
#[wasm_bindgen]
pub fn is_subset_of(child_json: &str, parent_json: &str) -> bool {
    let parent: MethodCapability = match serde_json::from_str(parent_json) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let child: MethodCapability = match serde_json::from_str(child_json) {
        Ok(c) => c,
        Err(_) => return false,
    };

    child.is_subset_of(&parent)
}

/// Check if a method name matches a glob pattern.
///
/// # Arguments
///
/// * `method` - The method name to check
/// * `pattern` - The glob pattern
///
/// # Returns
///
/// Returns `true` if the method matches the pattern.
#[wasm_bindgen]
pub fn method_matches_pattern(method: &str, pattern: &str) -> bool {
    patterns::method_matches_pattern(method, pattern)
}

/// Check if a child pattern is a subset of a parent pattern.
///
/// # Arguments
///
/// * `child_pattern` - The child pattern
/// * `parent_pattern` - The parent pattern
///
/// # Returns
///
/// Returns `true` if child pattern is a subset of parent pattern.
#[wasm_bindgen]
pub fn pattern_is_subset(child_pattern: &str, parent_pattern: &str) -> bool {
    patterns::pattern_is_subset(child_pattern, parent_pattern)
}

/// Check if a method call would be allowed by a capability.
///
/// Convenience function that returns a boolean instead of an error.
///
/// # Arguments
///
/// * `capability_json` - JSON-serialized `MethodCapability`
/// * `method` - The method name being called
/// * `params_json` - JSON-serialized parameters
///
/// # Returns
///
/// Returns `true` if the call is allowed.
#[wasm_bindgen]
pub fn can_call(capability_json: &str, method: &str, params_json: &str) -> bool {
    validate_method_call(capability_json, method, params_json).is_ok()
}

/// Create a new method capability JSON.
///
/// # Arguments
///
/// * `method_pattern` - The glob pattern for method names
///
/// # Returns
///
/// Returns JSON-serialized `MethodCapability`.
#[wasm_bindgen]
pub fn create_method_capability(method_pattern: &str) -> Result<String, JsValue> {
    let cap = MethodCapability::new(method_pattern);
    serde_json::to_string(&cap).map_err(|e| JsValue::from_str(&format!("serialization error: {e}")))
}

/// Get the key for a method capability.
///
/// # Arguments
///
/// * `capability_json` - JSON-serialized `MethodCapability`
///
/// # Returns
///
/// Returns the capability key (e.g., "cap:method:stripe/charges/*").
#[wasm_bindgen]
pub fn get_capability_key(capability_json: &str) -> Result<String, JsValue> {
    let cap: MethodCapability = serde_json::from_str(capability_json)
        .map_err(|e| JsValue::from_str(&format!("invalid capability JSON: {e}")))?;

    Ok(cap.key())
}
