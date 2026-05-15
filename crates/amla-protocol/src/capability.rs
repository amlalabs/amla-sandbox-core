//! Capability types - opaque to the protocol layer.
//!
//! The protocol treats capabilities as opaque data with a type tag.
//! Capability semantics (what they mean, how attenuation works) are
//! defined by the application layer via [`TransitionValidator`].
//!
//! # Design: Protocol vs Application Layer Separation
//!
//! This module intentionally keeps capabilities opaque:
//!
//! | Concern | Handled By |
//! |---------|-----------|
//! | Key matching | Protocol (chain validation) |
//! | Type routing | Protocol (to validator) |
//! | Attenuation rules | Application (`TransitionValidator`) |
//! | Payload schema | Application (defined per type) |
//!
//! This separation enables:
//! - **Generic protocol**: Same chain validation for any capability type
//! - **Extensibility**: New capability types without protocol changes
//! - **Type safety**: Application layer uses strongly-typed structs
//!
//! # The `key` Field
//!
//! The `key` uniquely identifies a capability within a PCA. It's used for
//! parent-child matching during chain validation (see [`crate::chain`]).
//! Convention: use namespaced keys like `"cap:payments.refund"` or `"cap:db.read"`.

use std::sync::Arc;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};
use crate::serialization::{canonical_cbor_encode, cbor_decode};

/// Maximum size for a capability payload in bytes (1 MB).
///
/// This limit prevents denial-of-service attacks through excessively large capability
/// payloads during chain validation. 1 MB is generous for any reasonable capability
/// data while still providing protection.
pub const MAX_CAPABILITY_PAYLOAD_SIZE: usize = 1024 * 1024;

/// Opaque capability data at the protocol layer.
///
/// The protocol does not interpret the contents - it just:
/// 1. Carries the key for matching across chain hops
/// 2. Carries the type tag for routing to validators
/// 3. Stores the payload as opaque CBOR bytes
///
/// # Example
///
/// ```
/// use amla_protocol::CapabilityData;
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct FunctionCap {
///     name: String,
///     max_amount: u64,
/// }
///
/// // Create from typed payload
/// let payload = FunctionCap { name: "payments.refund".into(), max_amount: 500 };
/// let cap = CapabilityData::new("cap:refund", "function", &payload).unwrap();
///
/// assert_eq!(cap.key(), "cap:refund");
/// assert_eq!(cap.capability_type(), "function");
///
/// // Decode back to typed struct
/// let decoded: FunctionCap = cap.decode().unwrap();
/// assert_eq!(decoded.name, "payments.refund");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityData {
    /// Stable key for matching capabilities across chain hops.
    ///
    /// Used by chain validators to match parent/child capabilities.
    /// Convention: namespaced keys like "cap:refunds", "cap:read".
    key: String,

    /// Type tag for routing to the correct validator.
    ///
    /// Convention: lowercase, hyphenated (e.g., "function", "resource-access")
    #[serde(rename = "type")]
    capability_type: String,

    /// CBOR-encoded payload. Opaque to the protocol layer.
    ///
    /// The application layer defines the schema and semantics.
    #[serde(with = "serde_bytes")]
    data: Vec<u8>,
}

impl CapabilityData {
    /// Create a new capability from a serializable payload.
    ///
    /// The payload is encoded to canonical CBOR bytes.
    ///
    /// # Arguments
    ///
    /// * `key` - Stable identifier for matching across chain hops
    /// * `capability_type` - Type tag for routing to validators
    /// * `payload` - Application-defined data (serialized to CBOR)
    ///
    /// # Errors
    ///
    /// Returns an error if CBOR serialization fails or payload exceeds
    /// [`MAX_CAPABILITY_PAYLOAD_SIZE`].
    #[must_use = "this returns a Result that may contain an error"]
    pub fn new<T: Serialize>(key: &str, capability_type: &str, payload: &T) -> Result<Self> {
        let data = canonical_cbor_encode(payload)?;
        if data.len() > MAX_CAPABILITY_PAYLOAD_SIZE {
            return Err(Error::PayloadTooLarge {
                size: data.len(),
                limit: MAX_CAPABILITY_PAYLOAD_SIZE,
            });
        }
        Ok(Self {
            key: key.to_string(),
            capability_type: capability_type.to_string(),
            data,
        })
    }

    /// Create from raw CBOR bytes.
    ///
    /// Use this when you already have CBOR-encoded data.
    ///
    /// # Errors
    ///
    /// Returns an error if the data exceeds [`MAX_CAPABILITY_PAYLOAD_SIZE`].
    pub fn from_cbor_bytes(key: &str, capability_type: &str, data: Vec<u8>) -> Result<Self> {
        if data.len() > MAX_CAPABILITY_PAYLOAD_SIZE {
            return Err(Error::PayloadTooLarge {
                size: data.len(),
                limit: MAX_CAPABILITY_PAYLOAD_SIZE,
            });
        }
        Ok(Self {
            key: key.to_string(),
            capability_type: capability_type.to_string(),
            data,
        })
    }

    /// Create from a JSON value (convenience for JavaScript interop).
    ///
    /// The JSON is converted to CBOR internally.
    ///
    /// # Errors
    ///
    /// Returns an error if CBOR serialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn from_json(key: &str, capability_type: &str, json: &serde_json::Value) -> Result<Self> {
        Self::new(key, capability_type, json)
    }

    /// Get the capability key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Get the capability type tag.
    #[must_use]
    pub fn capability_type(&self) -> &str {
        &self.capability_type
    }

    /// Get the raw CBOR bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Decode the payload into a typed struct.
    ///
    /// # Errors
    ///
    /// Returns an error if CBOR deserialization fails or type doesn't match.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        cbor_decode(&self.data)
    }

    /// Decode to a JSON value (convenience for JavaScript interop).
    ///
    /// # Errors
    ///
    /// Returns an error if CBOR deserialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn to_json(&self) -> Result<serde_json::Value> {
        self.decode()
    }
}

/// Check for duplicate capability keys in a slice.
///
/// Returns `Some(key)` if a duplicate is found, `None` otherwise.
/// This is a helper function used by PCA building and chain validation.
///
/// # Example
///
/// ```
/// use amla_protocol::{CapabilityData, find_duplicate_capability_key};
/// use serde_json::json;
///
/// let cap1 = CapabilityData::from_json("cap:a", "type", &json!({})).unwrap();
/// let cap2 = CapabilityData::from_json("cap:b", "type", &json!({})).unwrap();
/// let cap3 = CapabilityData::from_json("cap:a", "type", &json!({})).unwrap(); // duplicate!
///
/// assert!(find_duplicate_capability_key(&[cap1.clone(), cap2.clone()]).is_none());
/// assert_eq!(find_duplicate_capability_key(&[cap1, cap2, cap3]), Some("cap:a"));
/// ```
#[must_use]
pub fn find_duplicate_capability_key(caps: &[CapabilityData]) -> Option<&str> {
    let mut seen = std::collections::HashSet::with_capacity(caps.len());
    for cap in caps {
        if !seen.insert(cap.key()) {
            return Some(cap.key());
        }
    }
    None
}

/// Error returned when a capability transition is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    /// Human-readable reason for the failure.
    pub reason: String,
}

impl TransitionError {
    /// Create a new transition error.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for TransitionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid capability transition: {}", self.reason)
    }
}

impl std::error::Error for TransitionError {}

/// Validates capability transitions (attenuation).
///
/// The protocol layer calls this to check if a child capability
/// is a valid attenuation of a parent capability.
///
/// # Implementing
///
/// Applications implement this trait to define their capability semantics:
///
/// ```
/// use amla_protocol::{CapabilityData, TransitionValidator, TransitionError};
/// use serde::{Serialize, Deserialize};
///
/// #[derive(Serialize, Deserialize)]
/// struct FunctionCap {
///     name: String,
///     max_amount: u64,
/// }
///
/// struct FunctionValidator;
///
/// impl TransitionValidator for FunctionValidator {
///     fn validate_transition(
///         &self,
///         parent: &CapabilityData,
///         child: &CapabilityData,
///     ) -> Result<(), TransitionError> {
///         // Must be same type
///         if parent.capability_type() != child.capability_type() {
///             return Err(TransitionError::new("capability type mismatch"));
///         }
///
///         // Decode and check attenuation
///         let parent_cap: FunctionCap = parent.decode()
///             .map_err(|e| TransitionError::new(format!("decode parent: {e}")))?;
///         let child_cap: FunctionCap = child.decode()
///             .map_err(|e| TransitionError::new(format!("decode child: {e}")))?;
///
///         if child_cap.max_amount > parent_cap.max_amount {
///             return Err(TransitionError::new("child max_amount exceeds parent"));
///         }
///
///         Ok(())
///     }
/// }
/// ```
pub trait TransitionValidator: Send + Sync {
    /// Validate that `child` is a valid attenuation of `parent`.
    ///
    /// # Returns
    ///
    /// - `Ok(())` if the transition is valid (child ⊆ parent)
    /// - `Err(TransitionError)` if the transition is invalid
    ///
    /// # Contract
    ///
    /// - If this returns `Ok`, then any action allowed by `child`
    ///   must also be allowed by `parent`.
    /// - Conservative: May reject valid transitions (safe).
    /// - Must NOT accept invalid transitions (unsafe).
    fn validate_transition(
        &self,
        parent: &CapabilityData,
        child: &CapabilityData,
    ) -> std::result::Result<(), TransitionError>;
}

/// A permissive validator that accepts all transitions.
///
/// **WARNING**: Only use this for testing or when transition
/// validation is handled elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct PermissiveValidator;

impl TransitionValidator for PermissiveValidator {
    fn validate_transition(
        &self,
        _parent: &CapabilityData,
        _child: &CapabilityData,
    ) -> std::result::Result<(), TransitionError> {
        Ok(())
    }
}

/// A strict validator that only accepts identical capabilities.
///
/// Useful as a baseline - no attenuation allowed, only exact copies.
#[derive(Debug, Clone, Copy, Default)]
pub struct StrictValidator;

impl TransitionValidator for StrictValidator {
    fn validate_transition(
        &self,
        parent: &CapabilityData,
        child: &CapabilityData,
    ) -> std::result::Result<(), TransitionError> {
        if parent == child {
            Ok(())
        } else {
            Err(TransitionError::new("capabilities must be identical"))
        }
    }
}

/// A validator that delegates to a closure.
///
/// Useful for simple validation logic without defining a full struct.
pub struct ClosureValidator<F>
where
    F: Fn(&CapabilityData, &CapabilityData) -> std::result::Result<(), TransitionError>
        + Send
        + Sync,
{
    validate_fn: F,
}

impl<F> ClosureValidator<F>
where
    F: Fn(&CapabilityData, &CapabilityData) -> std::result::Result<(), TransitionError>
        + Send
        + Sync,
{
    /// Create a new closure-based validator.
    pub fn new(validate_fn: F) -> Self {
        Self { validate_fn }
    }
}

impl<F> TransitionValidator for ClosureValidator<F>
where
    F: Fn(&CapabilityData, &CapabilityData) -> std::result::Result<(), TransitionError>
        + Send
        + Sync,
{
    fn validate_transition(
        &self,
        parent: &CapabilityData,
        child: &CapabilityData,
    ) -> std::result::Result<(), TransitionError> {
        (self.validate_fn)(parent, child)
    }
}

/// Routes validation to type-specific validators based on capability type.
///
/// Use when your application has multiple capability types (e.g., "function",
/// "resource-access", "budget") with different attenuation semantics.
///
/// This type is cloneable - cloned instances share the same underlying validators.
///
/// # Example
///
/// ```
/// use amla_protocol::{
///     CapabilityData, TransitionValidator, TransitionError,
///     TypeDispatchValidator, ClosureValidator,
/// };
///
/// // Create per-type validators
/// let function_validator = ClosureValidator::new(|parent, child| {
///     // Function capabilities: check max_amount attenuation
///     let parent_data: serde_json::Value = parent.to_json().unwrap();
///     let child_data: serde_json::Value = child.to_json().unwrap();
///     let parent_max = parent_data["max_amount"].as_i64().unwrap_or(i64::MAX);
///     let child_max = child_data["max_amount"].as_i64().unwrap_or(i64::MAX);
///     if child_max > parent_max {
///         return Err(TransitionError::new("max_amount cannot increase"));
///     }
///     Ok(())
/// });
///
/// let resource_validator = ClosureValidator::new(|parent, child| {
///     // Resource capabilities: check path prefix
///     let parent_path = parent.to_json().unwrap()["path"].as_str().unwrap_or("/").to_string();
///     let child_path = child.to_json().unwrap()["path"].as_str().unwrap_or("/").to_string();
///     if !child_path.starts_with(&parent_path) {
///         return Err(TransitionError::new("path must be within parent scope"));
///     }
///     Ok(())
/// });
///
/// // Build dispatcher
/// let validator = TypeDispatchValidator::new()
///     .register("function", function_validator)
///     .register("resource-access", resource_validator);
///
/// // Cloning shares the validators
/// let validator2 = validator.clone();
///
/// // Use with CTA
/// // let cta = CtaBuilder::new(keypair).validator(validator).build();
/// ```
#[derive(Clone)]
pub struct TypeDispatchValidator {
    validators: std::collections::HashMap<String, Arc<dyn TransitionValidator>>,
    /// Fallback for unknown types: if None, unknown types are rejected.
    fallback: Option<Arc<dyn TransitionValidator>>,
}

impl Default for TypeDispatchValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeDispatchValidator {
    /// Create a new empty dispatcher.
    ///
    /// By default, unknown capability types are rejected.
    #[must_use]
    pub fn new() -> Self {
        Self {
            validators: std::collections::HashMap::new(),
            fallback: None,
        }
    }

    /// Register a validator for a specific capability type.
    ///
    /// The type should match `CapabilityData::capability_type()`.
    #[must_use]
    pub fn register<V: TransitionValidator + 'static>(
        mut self,
        capability_type: &str,
        validator: V,
    ) -> Self {
        self.validators
            .insert(capability_type.to_string(), Arc::new(validator));
        self
    }

    /// Set a fallback validator for unknown capability types.
    ///
    /// Without a fallback, unknown types are rejected.
    #[must_use]
    pub fn with_fallback<V: TransitionValidator + 'static>(mut self, validator: V) -> Self {
        self.fallback = Some(Arc::new(validator));
        self
    }
}

impl TransitionValidator for TypeDispatchValidator {
    fn validate_transition(
        &self,
        parent: &CapabilityData,
        child: &CapabilityData,
    ) -> std::result::Result<(), TransitionError> {
        // Types must match
        if parent.capability_type() != child.capability_type() {
            return Err(TransitionError::new(format!(
                "capability type mismatch: parent='{}', child='{}'",
                parent.capability_type(),
                child.capability_type()
            )));
        }

        let cap_type = parent.capability_type();

        // Look up validator for this type
        if let Some(validator) = self.validators.get(cap_type) {
            validator.validate_transition(parent, child)
        } else if let Some(fallback) = &self.fallback {
            fallback.validate_transition(parent, child)
        } else {
            Err(TransitionError::new(format!(
                "no validator registered for capability type '{cap_type}'"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capability_data_creation() {
        let cap = CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "test.func"}),
        )
        .unwrap();

        assert_eq!(cap.key(), "cap:test");
        assert_eq!(cap.capability_type(), "function");

        let json = cap.to_json().unwrap();
        assert_eq!(json.get("name").unwrap(), "test.func");
    }

    #[test]
    fn test_capability_data_typed_roundtrip() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct TestPayload {
            name: String,
            value: i64,
        }

        let original = TestPayload {
            name: "test".to_string(),
            value: 42,
        };

        let cap = CapabilityData::new("cap:typed", "test", &original).unwrap();
        let decoded: TestPayload = cap.decode().unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_capability_data_json_roundtrip() {
        let json = serde_json::json!({
            "path": "/api/users",
            "methods": ["GET", "POST"]
        });

        let cap = CapabilityData::from_json("cap:resource", "resource", &json).unwrap();
        let decoded = cap.to_json().unwrap();

        assert_eq!(json, decoded);
    }

    #[test]
    fn test_capability_serialization() {
        let cap =
            CapabilityData::from_json("cap:test", "function", &serde_json::json!({"name": "test"}))
                .unwrap();

        // Serialize to CBOR
        let cbor = canonical_cbor_encode(&cap).unwrap();

        // Deserialize
        let cap2: CapabilityData = cbor_decode(&cbor).unwrap();

        assert_eq!(cap, cap2);
    }

    #[test]
    fn test_permissive_validator() {
        let validator = PermissiveValidator;

        let parent = CapabilityData::from_json("cap:a", "a", &serde_json::json!({})).unwrap();
        let child =
            CapabilityData::from_json("cap:b", "b", &serde_json::json!({"different": true}))
                .unwrap();

        // Accepts everything
        assert!(validator.validate_transition(&parent, &child).is_ok());
    }

    #[test]
    fn test_strict_validator() {
        let validator = StrictValidator;

        let cap1 =
            CapabilityData::from_json("cap:func", "func", &serde_json::json!({"x": 1})).unwrap();
        let cap2 =
            CapabilityData::from_json("cap:func", "func", &serde_json::json!({"x": 1})).unwrap();
        let cap3 =
            CapabilityData::from_json("cap:func", "func", &serde_json::json!({"x": 2})).unwrap();

        // Same capability -> OK
        assert!(validator.validate_transition(&cap1, &cap2).is_ok());

        // Different capability -> Error
        assert!(validator.validate_transition(&cap1, &cap3).is_err());
    }

    #[test]
    fn test_closure_validator() {
        // Only allow if child has same type
        let validator = ClosureValidator::new(|parent, child| {
            if parent.capability_type() != child.capability_type() {
                return Err(TransitionError::new("type mismatch"));
            }
            Ok(())
        });

        let parent =
            CapabilityData::from_json("cap:test", "test", &serde_json::json!({"a": 1})).unwrap();
        let valid_child =
            CapabilityData::from_json("cap:test", "test", &serde_json::json!({"a": 2})).unwrap();
        let invalid_child =
            CapabilityData::from_json("cap:test", "other", &serde_json::json!({"a": 1})).unwrap();

        assert!(validator.validate_transition(&parent, &valid_child).is_ok());
        assert!(
            validator
                .validate_transition(&parent, &invalid_child)
                .is_err()
        );
    }

    #[test]
    fn test_cbor_bytes_not_double_encoded() {
        // Create a capability with some data
        let cap =
            CapabilityData::from_json("cap:test", "function", &serde_json::json!({"value": 42}))
                .unwrap();

        // Serialize the capability
        let cbor = canonical_cbor_encode(&cap).unwrap();

        // The data field should be a bstr (major type 2), not an array
        // Parse to ciborium::Value to inspect
        let value: ciborium::Value = cbor_decode(&cbor).unwrap();

        if let ciborium::Value::Map(map) = value {
            for (k, v) in &map {
                if let ciborium::Value::Text(key) = k
                    && key == "data"
                {
                    // Should be Bytes, not Array
                    assert!(
                        matches!(v, ciborium::Value::Bytes(_)),
                        "data should be bstr, got: {v:?}"
                    );
                }
            }
        } else {
            panic!("expected map");
        }
    }

    #[test]
    fn test_from_cbor_bytes() {
        // Create CBOR data directly
        let raw_cbor = canonical_cbor_encode(&serde_json::json!({"test": "value"})).unwrap();

        let cap = CapabilityData::from_cbor_bytes("cap:raw", "raw-type", raw_cbor.clone()).unwrap();

        assert_eq!(cap.key(), "cap:raw");
        assert_eq!(cap.capability_type(), "raw-type");
        assert_eq!(cap.as_bytes(), raw_cbor.as_slice());

        // Verify we can decode it back
        let decoded: serde_json::Value = cap.decode().unwrap();
        assert_eq!(decoded, serde_json::json!({"test": "value"}));
    }

    #[test]
    fn test_capability_payload_size_limit() {
        // Create a payload that exceeds the size limit
        let large_data = vec![0u8; MAX_CAPABILITY_PAYLOAD_SIZE + 1];

        // from_cbor_bytes should reject oversized payloads
        let result = CapabilityData::from_cbor_bytes("cap:large", "test", large_data);
        assert!(result.is_err());

        // Verify the error type
        if let Err(crate::Error::PayloadTooLarge { size, limit }) = result {
            assert_eq!(size, MAX_CAPABILITY_PAYLOAD_SIZE + 1);
            assert_eq!(limit, MAX_CAPABILITY_PAYLOAD_SIZE);
        } else {
            panic!("expected PayloadTooLarge error");
        }
    }

    #[test]
    fn test_as_bytes_returns_raw_cbor() {
        let cap = CapabilityData::from_json("cap:test", "function", &serde_json::json!({"x": 123}))
            .unwrap();

        // as_bytes should return the CBOR-encoded payload
        let bytes = cap.as_bytes();
        assert!(!bytes.is_empty());

        // Should be valid CBOR
        let decoded: serde_json::Value = cbor_decode(bytes).unwrap();
        assert_eq!(decoded, serde_json::json!({"x": 123}));
    }

    #[test]
    fn test_transition_error_display() {
        let err = TransitionError::new("privilege escalation detected");
        let display = format!("{err}");
        assert_eq!(
            display,
            "invalid capability transition: privilege escalation detected"
        );

        // Also test Debug
        let debug = format!("{err:?}");
        assert!(debug.contains("privilege escalation detected"));
    }

    #[test]
    fn test_transition_error_is_std_error() {
        // TransitionError implements std::error::Error
        fn accepts_error(_: &dyn std::error::Error) {}

        let err = TransitionError::new("test error");
        accepts_error(&err);
    }

    // =========================================================================
    // TypeDispatchValidator Tests
    // =========================================================================

    #[test]
    fn test_type_dispatch_validator_routes_by_type() {
        // Create validators for different capability types
        let validator = TypeDispatchValidator::new()
            .register("function", PermissiveValidator)
            .register("resource", StrictValidator);

        // Function type uses PermissiveValidator (accepts any transition)
        let func_parent =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"a": 1})).unwrap();
        let func_child =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"a": 2})).unwrap();
        assert!(
            validator
                .validate_transition(&func_parent, &func_child)
                .is_ok()
        );

        // Resource type uses StrictValidator (requires identical data)
        let res_parent =
            CapabilityData::from_json("cap:res", "resource", &serde_json::json!({"path": "/x"}))
                .unwrap();
        let res_child_same =
            CapabilityData::from_json("cap:res", "resource", &serde_json::json!({"path": "/x"}))
                .unwrap();
        let res_child_diff =
            CapabilityData::from_json("cap:res", "resource", &serde_json::json!({"path": "/y"}))
                .unwrap();

        assert!(
            validator
                .validate_transition(&res_parent, &res_child_same)
                .is_ok()
        );
        assert!(
            validator
                .validate_transition(&res_parent, &res_child_diff)
                .is_err()
        );
    }

    #[test]
    fn test_type_dispatch_validator_rejects_type_mismatch() {
        let validator = TypeDispatchValidator::new()
            .register("function", PermissiveValidator)
            .register("resource", PermissiveValidator);

        let parent =
            CapabilityData::from_json("cap:test", "function", &serde_json::json!({})).unwrap();
        let child =
            CapabilityData::from_json("cap:test", "resource", &serde_json::json!({})).unwrap();

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("type mismatch"));
    }

    #[test]
    fn test_type_dispatch_validator_rejects_unknown_type() {
        let validator = TypeDispatchValidator::new().register("function", PermissiveValidator);

        let parent =
            CapabilityData::from_json("cap:test", "unknown", &serde_json::json!({})).unwrap();
        let child =
            CapabilityData::from_json("cap:test", "unknown", &serde_json::json!({})).unwrap();

        let result = validator.validate_transition(&parent, &child);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("no validator registered"));
    }

    #[test]
    fn test_type_dispatch_validator_fallback() {
        let validator = TypeDispatchValidator::new()
            .register("function", StrictValidator)
            .with_fallback(PermissiveValidator);

        // Registered type uses its validator
        let func_parent =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"a": 1})).unwrap();
        let func_child =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"a": 2})).unwrap();
        assert!(
            validator
                .validate_transition(&func_parent, &func_child)
                .is_err()
        ); // StrictValidator rejects

        // Unknown type uses fallback
        let other_parent =
            CapabilityData::from_json("cap:other", "custom", &serde_json::json!({"x": 1})).unwrap();
        let other_child =
            CapabilityData::from_json("cap:other", "custom", &serde_json::json!({"x": 99}))
                .unwrap();
        assert!(
            validator
                .validate_transition(&other_parent, &other_child)
                .is_ok()
        ); // PermissiveValidator accepts
    }

    #[test]
    fn test_type_dispatch_validator_with_closure() {
        // Custom validator that checks max_amount attenuation
        let amount_validator = ClosureValidator::new(|parent, child| {
            let p_data: serde_json::Value = parent.decode().unwrap();
            let c_data: serde_json::Value = child.decode().unwrap();

            let p_amount = p_data
                .get("max_amount")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let c_amount = c_data
                .get("max_amount")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);

            if c_amount > p_amount {
                return Err(TransitionError::new("child max_amount exceeds parent"));
            }
            Ok(())
        });

        let validator = TypeDispatchValidator::new().register("payment", amount_validator);

        // Valid: child reduces max_amount
        let parent = CapabilityData::from_json(
            "cap:pay",
            "payment",
            &serde_json::json!({"max_amount": 1000}),
        )
        .unwrap();
        let child = CapabilityData::from_json(
            "cap:pay",
            "payment",
            &serde_json::json!({"max_amount": 500}),
        )
        .unwrap();
        assert!(validator.validate_transition(&parent, &child).is_ok());

        // Invalid: child increases max_amount (privilege escalation!)
        let escalated = CapabilityData::from_json(
            "cap:pay",
            "payment",
            &serde_json::json!({"max_amount": 2000}),
        )
        .unwrap();
        assert!(validator.validate_transition(&parent, &escalated).is_err());
    }

    #[test]
    fn test_type_dispatch_validator_clone() {
        // Verify that TypeDispatchValidator can be cloned and both copies work
        let validator = TypeDispatchValidator::new()
            .register("function", PermissiveValidator)
            .with_fallback(StrictValidator);

        // Clone the validator
        let validator2 = validator.clone();

        let parent =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"x": 1})).unwrap();
        let child =
            CapabilityData::from_json("cap:fn", "function", &serde_json::json!({"x": 2})).unwrap();

        // Both validators should work identically
        assert!(validator.validate_transition(&parent, &child).is_ok());
        assert!(validator2.validate_transition(&parent, &child).is_ok());
    }

    // =========================================================================
    // Complex Attenuation Pattern Tests
    // =========================================================================

    #[test]
    fn test_attenuation_nested_json_field_reduction() {
        // Validator that enforces nested constraint reduction
        let nested_validator = ClosureValidator::new(|parent, child| {
            let p: serde_json::Value = parent.decode().unwrap();
            let c: serde_json::Value = child.decode().unwrap();

            // Check constraints.max_amount is not increased
            let p_max = p
                .pointer("/constraints/max_amount")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            let c_max = c
                .pointer("/constraints/max_amount")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);

            if c_max > p_max {
                return Err(TransitionError::new("cannot increase max_amount"));
            }
            Ok(())
        });

        let parent = CapabilityData::from_json(
            "cap:api",
            "function",
            &serde_json::json!({
                "name": "payments.refund",
                "constraints": {
                    "max_amount": 50000,
                    "currencies": ["USD", "EUR"]
                }
            }),
        )
        .unwrap();

        // Valid: reduce max_amount
        let valid_child = CapabilityData::from_json(
            "cap:api",
            "function",
            &serde_json::json!({
                "name": "payments.refund",
                "constraints": {
                    "max_amount": 10000,
                    "currencies": ["USD"]
                }
            }),
        )
        .unwrap();

        // Invalid: increase max_amount
        let invalid_child = CapabilityData::from_json(
            "cap:api",
            "function",
            &serde_json::json!({
                "name": "payments.refund",
                "constraints": {
                    "max_amount": 100_000,
                    "currencies": ["USD", "EUR", "GBP"]
                }
            }),
        )
        .unwrap();

        assert!(
            nested_validator
                .validate_transition(&parent, &valid_child)
                .is_ok()
        );
        assert!(
            nested_validator
                .validate_transition(&parent, &invalid_child)
                .is_err()
        );
    }

    #[test]
    fn test_attenuation_array_subset() {
        // Validator that enforces array subset (child must be subset of parent)
        let subset_validator = ClosureValidator::new(|parent, child| {
            let p: serde_json::Value = parent.decode().unwrap();
            let c: serde_json::Value = child.decode().unwrap();

            let p_allowed: std::collections::HashSet<&str> = p
                .get("allowed_operations")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            let c_allowed: std::collections::HashSet<&str> = c
                .get("allowed_operations")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();

            if !c_allowed.is_subset(&p_allowed) {
                return Err(TransitionError::new(
                    "child operations must be subset of parent",
                ));
            }
            Ok(())
        });

        let parent = CapabilityData::from_json(
            "cap:db",
            "database",
            &serde_json::json!({
                "table": "users",
                "allowed_operations": ["SELECT", "INSERT", "UPDATE"]
            }),
        )
        .unwrap();

        // Valid: subset of operations
        let valid_child = CapabilityData::from_json(
            "cap:db",
            "database",
            &serde_json::json!({
                "table": "users",
                "allowed_operations": ["SELECT"]
            }),
        )
        .unwrap();

        // Invalid: adds DELETE operation
        let invalid_child = CapabilityData::from_json(
            "cap:db",
            "database",
            &serde_json::json!({
                "table": "users",
                "allowed_operations": ["SELECT", "DELETE"]
            }),
        )
        .unwrap();

        assert!(
            subset_validator
                .validate_transition(&parent, &valid_child)
                .is_ok()
        );
        assert!(
            subset_validator
                .validate_transition(&parent, &invalid_child)
                .is_err()
        );
    }

    #[test]
    fn test_attenuation_time_window_narrowing() {
        // Validator for time-bounded capabilities
        let time_validator = ClosureValidator::new(|parent, child| {
            let p: serde_json::Value = parent.decode().unwrap();
            let c: serde_json::Value = child.decode().unwrap();

            let p_start = p
                .get("valid_from")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let p_end = p
                .get("valid_until")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);

            let c_start = c
                .get("valid_from")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let c_end = c
                .get("valid_until")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);

            // Child window must be within parent window
            if c_start < p_start || c_end > p_end {
                return Err(TransitionError::new(
                    "child time window must be within parent",
                ));
            }
            Ok(())
        });

        let parent = CapabilityData::from_json(
            "cap:access",
            "temporal",
            &serde_json::json!({
                "resource": "/api/data",
                "valid_from": 1000,
                "valid_until": 5000
            }),
        )
        .unwrap();

        // Valid: narrower window
        let valid_child = CapabilityData::from_json(
            "cap:access",
            "temporal",
            &serde_json::json!({
                "resource": "/api/data",
                "valid_from": 2000,
                "valid_until": 4000
            }),
        )
        .unwrap();

        // Invalid: starts earlier than parent
        let invalid_start = CapabilityData::from_json(
            "cap:access",
            "temporal",
            &serde_json::json!({
                "resource": "/api/data",
                "valid_from": 500,
                "valid_until": 4000
            }),
        )
        .unwrap();

        // Invalid: ends later than parent
        let invalid_end = CapabilityData::from_json(
            "cap:access",
            "temporal",
            &serde_json::json!({
                "resource": "/api/data",
                "valid_from": 2000,
                "valid_until": 6000
            }),
        )
        .unwrap();

        assert!(
            time_validator
                .validate_transition(&parent, &valid_child)
                .is_ok()
        );
        assert!(
            time_validator
                .validate_transition(&parent, &invalid_start)
                .is_err()
        );
        assert!(
            time_validator
                .validate_transition(&parent, &invalid_end)
                .is_err()
        );
    }

    #[test]
    fn test_attenuation_multiple_constraints() {
        // Composite validator that checks multiple constraints
        let multi_validator = ClosureValidator::new(|parent, child| {
            let p: serde_json::Value = parent.decode().unwrap();
            let c: serde_json::Value = child.decode().unwrap();

            // 1. Check rate limit can only decrease
            let p_rate = p
                .get("rate_limit")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            let c_rate = c
                .get("rate_limit")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            if c_rate > p_rate {
                return Err(TransitionError::new("rate_limit cannot increase"));
            }

            // 2. Check scope must be subset (simple string prefix check)
            let p_scope = p.get("scope").and_then(|v| v.as_str()).unwrap_or("*");
            let c_scope = c.get("scope").and_then(|v| v.as_str()).unwrap_or("*");
            if !c_scope.starts_with(p_scope.trim_end_matches('*')) {
                return Err(TransitionError::new("scope must be narrower"));
            }

            // 3. Check read_only can only become true
            let p_readonly = p
                .get("read_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let c_readonly = c
                .get("read_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if p_readonly && !c_readonly {
                return Err(TransitionError::new("cannot remove read_only restriction"));
            }

            Ok(())
        });

        let parent = CapabilityData::from_json(
            "cap:api",
            "api-access",
            &serde_json::json!({
                "rate_limit": 1000,
                "scope": "/api/*",
                "read_only": false
            }),
        )
        .unwrap();

        // Valid: all constraints narrowed
        let valid = CapabilityData::from_json(
            "cap:api",
            "api-access",
            &serde_json::json!({
                "rate_limit": 100,
                "scope": "/api/users/*",
                "read_only": true
            }),
        )
        .unwrap();
        assert!(multi_validator.validate_transition(&parent, &valid).is_ok());

        // Invalid: rate_limit increased
        let invalid_rate = CapabilityData::from_json(
            "cap:api",
            "api-access",
            &serde_json::json!({
                "rate_limit": 5000,
                "scope": "/api/users/*",
                "read_only": true
            }),
        )
        .unwrap();
        assert!(
            multi_validator
                .validate_transition(&parent, &invalid_rate)
                .is_err()
        );

        // Create read_only parent for next test
        let readonly_parent = CapabilityData::from_json(
            "cap:api",
            "api-access",
            &serde_json::json!({
                "rate_limit": 1000,
                "scope": "/api/*",
                "read_only": true
            }),
        )
        .unwrap();

        // Invalid: trying to remove read_only
        let invalid_readonly = CapabilityData::from_json(
            "cap:api",
            "api-access",
            &serde_json::json!({
                "rate_limit": 100,
                "scope": "/api/users/*",
                "read_only": false
            }),
        )
        .unwrap();
        assert!(
            multi_validator
                .validate_transition(&readonly_parent, &invalid_readonly)
                .is_err()
        );
    }

    #[test]
    fn test_type_dispatch_default_impl() {
        // Verify Default is implemented
        let validator: TypeDispatchValidator = TypeDispatchValidator::default();

        let parent =
            CapabilityData::from_json("cap:test", "unknown", &serde_json::json!({})).unwrap();
        let child =
            CapabilityData::from_json("cap:test", "unknown", &serde_json::json!({})).unwrap();

        // Should fail - no validators registered
        assert!(validator.validate_transition(&parent, &child).is_err());
    }

    #[test]
    fn test_find_duplicate_capability_key_no_duplicates() {
        let caps = vec![
            CapabilityData::from_json("cap:a", "type", &serde_json::json!({})).unwrap(),
            CapabilityData::from_json("cap:b", "type", &serde_json::json!({})).unwrap(),
            CapabilityData::from_json("cap:c", "type", &serde_json::json!({})).unwrap(),
        ];
        assert!(find_duplicate_capability_key(&caps).is_none());
    }

    #[test]
    fn test_find_duplicate_capability_key_with_duplicate() {
        let caps = vec![
            CapabilityData::from_json("cap:a", "type", &serde_json::json!({})).unwrap(),
            CapabilityData::from_json("cap:b", "type", &serde_json::json!({})).unwrap(),
            CapabilityData::from_json("cap:a", "type", &serde_json::json!({})).unwrap(), // duplicate
        ];
        assert_eq!(find_duplicate_capability_key(&caps), Some("cap:a"));
    }

    #[test]
    fn test_find_duplicate_capability_key_empty() {
        let caps: Vec<CapabilityData> = vec![];
        assert!(find_duplicate_capability_key(&caps).is_none());
    }

    #[test]
    fn test_find_duplicate_capability_key_single() {
        let caps =
            vec![CapabilityData::from_json("cap:only", "type", &serde_json::json!({})).unwrap()];
        assert!(find_duplicate_capability_key(&caps).is_none());
    }
}

// Property-based tests (non-wasm only)
#[cfg(all(test, not(target_arch = "wasm32")))]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn capability_cbor_roundtrip(
            key in "[a-z:]{1,50}",
            type_tag in "[a-z-]{1,20}",
            payload_int in any::<i64>(),
            payload_str in "[a-zA-Z0-9]{0,100}"
        ) {
            let json = serde_json::json!({
                "value": payload_int,
                "name": payload_str
            });
            let cap = CapabilityData::from_json(&key, &type_tag, &json).unwrap();

            // CBOR roundtrip
            let cbor = crate::serialization::canonical_cbor_encode(&cap).unwrap();
            let decoded: CapabilityData = crate::serialization::cbor_decode(&cbor).unwrap();

            prop_assert_eq!(cap.key(), decoded.key());
            prop_assert_eq!(cap.capability_type(), decoded.capability_type());
            prop_assert_eq!(cap.as_bytes(), decoded.as_bytes());
        }

        #[test]
        fn capability_json_roundtrip(
            key in "[a-z:]{1,50}",
            type_tag in "[a-z-]{1,20}",
            value in any::<u32>()
        ) {
            let json = serde_json::json!({"amount": value});
            let cap = CapabilityData::from_json(&key, &type_tag, &json).unwrap();

            // Decode back to JSON and verify
            let decoded_json = cap.to_json().unwrap();
            prop_assert_eq!(&decoded_json["amount"], &serde_json::json!(value));
        }

        #[test]
        fn find_duplicate_returns_first_duplicate(
            unique_prefix in "[a-z]{1,5}",
            dup_key in "[a-z]{1,10}",
            num_before in 0usize..5,
            num_between in 0usize..5
        ) {
            let mut caps = Vec::new();

            // Add unique caps before
            for i in 0..num_before {
                let key = format!("{unique_prefix}:{i}");
                caps.push(
                    CapabilityData::from_json(&key, "type", &serde_json::json!({})).unwrap()
                );
            }

            // Add first occurrence
            let dup_full_key = format!("dup:{dup_key}");
            caps.push(
                CapabilityData::from_json(&dup_full_key, "type", &serde_json::json!({})).unwrap()
            );

            // Add unique caps between
            for i in 0..num_between {
                let key = format!("{unique_prefix}:between:{i}");
                caps.push(
                    CapabilityData::from_json(&key, "type", &serde_json::json!({})).unwrap()
                );
            }

            // Add duplicate
            caps.push(
                CapabilityData::from_json(&dup_full_key, "type", &serde_json::json!({})).unwrap()
            );

            let result = find_duplicate_capability_key(&caps);
            prop_assert_eq!(result, Some(dup_full_key.as_str()));
        }
    }
}
