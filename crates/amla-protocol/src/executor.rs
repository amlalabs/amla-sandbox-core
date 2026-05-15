//! Designated executor types for PCA authorization.
//!
//! The designated executor specifies who/what can continue a transaction
//! at the next hop in the chain.
//!
//! # Variants
//!
//! - [`DesignatedExecutor::PublicKey`]: Direct public key - the executor's identity is known.
//! - [`DesignatedExecutor::Characteristic`]: Characteristic-based - resolved by a CTA.
//! - [`DesignatedExecutor::CtaReference`]: Reference to another CTA for resolution.
//!
//! # Example
//!
//! ```
//! use amla_protocol::{DesignatedExecutor, KeyPair, Algorithm};
//!
//! let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
//!
//! // Direct public key designation (most common)
//! let executor = DesignatedExecutor::from_public_key(agent.public_key());
//!
//! // Check the variant
//! assert!(executor.is_public_key());
//! assert_eq!(executor.as_public_key(), Some(&agent.public_key()));
//! ```

use serde::{Deserialize, Serialize};

use crate::identity::PublicKey;

/// Who/what can continue this transaction at the next hop.
///
/// This is the core mechanism that binds authority to identity in the PIC model.
/// Possession of the chain is useless without being the designated executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
#[non_exhaustive]
pub enum DesignatedExecutor {
    /// Direct public key - the next executor's identity is known.
    ///
    /// Executor proves continuity by signing with this key.
    /// This is the most common variant for known successors.
    #[serde(rename = "pubkey")]
    PublicKey(PublicKey),

    /// Characteristic-based designation - resolved by a CTA.
    ///
    /// Example: "role:sales-agent", "workload:inference", "org:acme"
    /// The CTA uses an `ExecutorResolver` to verify the executor satisfies
    /// the characteristic.
    #[serde(rename = "characteristic")]
    Characteristic(ExecutorCharacteristic),

    /// Reference to a CTA that will resolve the actual executor.
    ///
    /// Used for cross-org delegation or when executor is unknown.
    /// The referenced CTA has discretion over executor selection.
    #[serde(rename = "cta_ref")]
    CtaReference(CtaReference),
}

impl DesignatedExecutor {
    /// Create a designated executor from a public key.
    #[must_use]
    pub fn from_public_key(key: PublicKey) -> Self {
        Self::PublicKey(key)
    }

    /// Create a designated executor from a characteristic.
    #[must_use]
    pub fn from_characteristic(characteristic: ExecutorCharacteristic) -> Self {
        Self::Characteristic(characteristic)
    }

    /// Create a designated executor from a CTA reference.
    #[must_use]
    pub fn from_cta_reference(cta_ref: CtaReference) -> Self {
        Self::CtaReference(cta_ref)
    }

    /// Check if this is a direct public key designation.
    #[must_use]
    pub fn is_public_key(&self) -> bool {
        matches!(self, Self::PublicKey(_))
    }

    /// Check if this is a characteristic-based designation.
    #[must_use]
    pub fn is_characteristic(&self) -> bool {
        matches!(self, Self::Characteristic(_))
    }

    /// Check if this is a CTA reference.
    #[must_use]
    pub fn is_cta_reference(&self) -> bool {
        matches!(self, Self::CtaReference(_))
    }

    /// Get the public key if this is a direct designation.
    #[must_use]
    pub fn as_public_key(&self) -> Option<&PublicKey> {
        match self {
            Self::PublicKey(pk) => Some(pk),
            _ => None,
        }
    }

    /// Get the characteristic if this is a characteristic-based designation.
    #[must_use]
    pub fn as_characteristic(&self) -> Option<&ExecutorCharacteristic> {
        match self {
            Self::Characteristic(c) => Some(c),
            _ => None,
        }
    }

    /// Get the CTA reference if this is a CTA reference designation.
    #[must_use]
    pub fn as_cta_reference(&self) -> Option<&CtaReference> {
        match self {
            Self::CtaReference(r) => Some(r),
            _ => None,
        }
    }
}

impl From<PublicKey> for DesignatedExecutor {
    fn from(key: PublicKey) -> Self {
        Self::PublicKey(key)
    }
}

impl From<ExecutorCharacteristic> for DesignatedExecutor {
    fn from(characteristic: ExecutorCharacteristic) -> Self {
        Self::Characteristic(characteristic)
    }
}

impl From<CtaReference> for DesignatedExecutor {
    fn from(cta_ref: CtaReference) -> Self {
        Self::CtaReference(cta_ref)
    }
}

/// Operational characteristics that define an executor without explicit identity.
///
/// Used for role-based or attribute-based designation where the specific
/// executor identity is determined at runtime by a CTA.
///
/// # Example
///
/// ```
/// use amla_protocol::ExecutorCharacteristic;
///
/// // Designate "any sales agent"
/// let char = ExecutorCharacteristic::new("role", "sales-agent");
///
/// // With additional constraints
/// let char_constrained = ExecutorCharacteristic::with_constraints(
///     "workload",
///     "inference",
///     vec![1, 2, 3], // Opaque CBOR constraints
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutorCharacteristic {
    /// Type of characteristic: "role", "workload", "org", "capability", etc.
    #[serde(rename = "char_type")]
    pub characteristic_type: String,

    /// The characteristic value/constraint.
    pub value: String,

    /// Optional: Additional constraints as opaque CBOR.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constraints: Option<Vec<u8>>,
}

impl ExecutorCharacteristic {
    /// Create a new characteristic without constraints.
    #[must_use]
    pub fn new(characteristic_type: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            characteristic_type: characteristic_type.into(),
            value: value.into(),
            constraints: None,
        }
    }

    /// Create a new characteristic with constraints.
    #[must_use]
    pub fn with_constraints(
        characteristic_type: impl Into<String>,
        value: impl Into<String>,
        constraints: Vec<u8>,
    ) -> Self {
        Self {
            characteristic_type: characteristic_type.into(),
            value: value.into(),
            constraints: Some(constraints),
        }
    }
}

/// Reference to a CTA for executor resolution.
///
/// Used when the executor identity should be determined by another CTA,
/// typically in cross-org scenarios or when the executor is unknown at
/// delegation time.
///
/// # Example
///
/// ```
/// use amla_protocol::{CtaReference, KeyPair, Algorithm};
///
/// let other_cta = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
///
/// // Reference to another CTA
/// let cta_ref = CtaReference::new(other_cta.public_key());
///
/// // With endpoint hint
/// let cta_ref_with_endpoint = CtaReference::with_endpoint(
///     other_cta.public_key(),
///     "https://cta.example.com/submit",
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CtaReference {
    /// CTA's public key for verification.
    pub cta_key: PublicKey,

    /// Optional: Endpoint hint for the CTA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Optional: Opaque payload describing what the CTA should do.
    /// Could contain: attenuation instructions, routing hints, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Vec<u8>>,
}

impl CtaReference {
    /// Create a new CTA reference.
    #[must_use]
    pub fn new(cta_key: PublicKey) -> Self {
        Self {
            cta_key,
            endpoint: None,
            payload: None,
        }
    }

    /// Create a CTA reference with an endpoint hint.
    #[must_use]
    pub fn with_endpoint(cta_key: PublicKey, endpoint: impl Into<String>) -> Self {
        Self {
            cta_key,
            endpoint: Some(endpoint.into()),
            payload: None,
        }
    }

    /// Create a CTA reference with both endpoint and payload.
    #[must_use]
    pub fn with_endpoint_and_payload(
        cta_key: PublicKey,
        endpoint: impl Into<String>,
        payload: Vec<u8>,
    ) -> Self {
        Self {
            cta_key,
            endpoint: Some(endpoint.into()),
            payload: Some(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serialization::{canonical_cbor_encode, cbor_decode};
    use crate::{Algorithm, KeyPair};

    #[test]
    fn test_designated_executor_public_key() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let executor = DesignatedExecutor::from_public_key(keypair.public_key());

        assert!(executor.is_public_key());
        assert!(!executor.is_characteristic());
        assert!(!executor.is_cta_reference());
        assert_eq!(executor.as_public_key(), Some(&keypair.public_key()));
        assert!(executor.as_characteristic().is_none());
        assert!(executor.as_cta_reference().is_none());
    }

    #[test]
    fn test_designated_executor_characteristic() {
        let char = ExecutorCharacteristic::new("role", "sales-agent");
        let executor = DesignatedExecutor::from_characteristic(char.clone());

        assert!(!executor.is_public_key());
        assert!(executor.is_characteristic());
        assert!(!executor.is_cta_reference());
        assert!(executor.as_public_key().is_none());
        assert_eq!(executor.as_characteristic(), Some(&char));
        assert!(executor.as_cta_reference().is_none());
    }

    #[test]
    fn test_designated_executor_cta_reference() {
        let cta = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let cta_ref = CtaReference::new(cta.public_key());
        let executor = DesignatedExecutor::from_cta_reference(cta_ref.clone());

        assert!(!executor.is_public_key());
        assert!(!executor.is_characteristic());
        assert!(executor.is_cta_reference());
        assert!(executor.as_public_key().is_none());
        assert!(executor.as_characteristic().is_none());
        assert_eq!(executor.as_cta_reference(), Some(&cta_ref));
    }

    #[test]
    fn test_designated_executor_from_impls() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

        // From PublicKey
        let executor: DesignatedExecutor = keypair.public_key().into();
        assert!(executor.is_public_key());

        // From ExecutorCharacteristic
        let char = ExecutorCharacteristic::new("role", "agent");
        let executor: DesignatedExecutor = char.into();
        assert!(executor.is_characteristic());

        // From CtaReference
        let cta_ref = CtaReference::new(keypair.public_key());
        let executor: DesignatedExecutor = cta_ref.into();
        assert!(executor.is_cta_reference());
    }

    #[test]
    fn test_executor_characteristic_construction() {
        let simple = ExecutorCharacteristic::new("role", "admin");
        assert_eq!(simple.characteristic_type, "role");
        assert_eq!(simple.value, "admin");
        assert!(simple.constraints.is_none());

        let with_constraints =
            ExecutorCharacteristic::with_constraints("workload", "inference", vec![1, 2, 3]);
        assert_eq!(with_constraints.characteristic_type, "workload");
        assert_eq!(with_constraints.value, "inference");
        assert_eq!(with_constraints.constraints, Some(vec![1, 2, 3]));
    }

    #[test]
    fn test_cta_reference_construction() {
        let cta = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

        let simple = CtaReference::new(cta.public_key());
        assert_eq!(simple.cta_key, cta.public_key());
        assert!(simple.endpoint.is_none());
        assert!(simple.payload.is_none());

        let with_endpoint =
            CtaReference::with_endpoint(cta.public_key(), "https://cta.example.com");
        assert_eq!(with_endpoint.cta_key, cta.public_key());
        assert_eq!(
            with_endpoint.endpoint,
            Some("https://cta.example.com".to_string())
        );
        assert!(with_endpoint.payload.is_none());

        let full = CtaReference::with_endpoint_and_payload(
            cta.public_key(),
            "https://cta.example.com",
            vec![4, 5, 6],
        );
        assert_eq!(full.cta_key, cta.public_key());
        assert_eq!(full.endpoint, Some("https://cta.example.com".to_string()));
        assert_eq!(full.payload, Some(vec![4, 5, 6]));
    }

    #[test]
    fn test_designated_executor_cbor_roundtrip_public_key() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let executor = DesignatedExecutor::from_public_key(keypair.public_key());

        let cbor = canonical_cbor_encode(&executor).unwrap();
        let decoded: DesignatedExecutor = cbor_decode(&cbor).unwrap();

        assert_eq!(executor, decoded);
    }

    #[test]
    fn test_designated_executor_cbor_roundtrip_characteristic() {
        let char = ExecutorCharacteristic::with_constraints("role", "agent", vec![1, 2, 3]);
        let executor = DesignatedExecutor::from_characteristic(char);

        let cbor = canonical_cbor_encode(&executor).unwrap();
        let decoded: DesignatedExecutor = cbor_decode(&cbor).unwrap();

        assert_eq!(executor, decoded);
    }

    #[test]
    fn test_designated_executor_cbor_roundtrip_cta_reference() {
        let cta = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let cta_ref = CtaReference::with_endpoint_and_payload(
            cta.public_key(),
            "https://cta.example.com",
            vec![7, 8, 9],
        );
        let executor = DesignatedExecutor::from_cta_reference(cta_ref);

        let cbor = canonical_cbor_encode(&executor).unwrap();
        let decoded: DesignatedExecutor = cbor_decode(&cbor).unwrap();

        assert_eq!(executor, decoded);
    }

    #[test]
    fn test_designated_executor_json_format() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let executor = DesignatedExecutor::from_public_key(keypair.public_key());

        let json = serde_json::to_string(&executor).unwrap();
        // Should have "type": "pubkey" tag
        assert!(json.contains(r#""type":"pubkey""#));

        let char = ExecutorCharacteristic::new("role", "agent");
        let executor = DesignatedExecutor::from_characteristic(char);
        let json = serde_json::to_string(&executor).unwrap();
        // Should have "type": "characteristic" tag
        assert!(json.contains(r#""type":"characteristic""#));

        let cta_ref = CtaReference::new(keypair.public_key());
        let executor = DesignatedExecutor::from_cta_reference(cta_ref);
        let json = serde_json::to_string(&executor).unwrap();
        // Should have "type": "cta_ref" tag
        assert!(json.contains(r#""type":"cta_ref""#));
    }

    #[test]
    fn test_executor_characteristic_equality() {
        let char1 = ExecutorCharacteristic::new("role", "admin");
        let char2 = ExecutorCharacteristic::new("role", "admin");
        let char3 = ExecutorCharacteristic::new("role", "user");

        assert_eq!(char1, char2);
        assert_ne!(char1, char3);
    }

    #[test]
    fn test_cta_reference_equality() {
        let cta1 = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let cta2 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let ref1 = CtaReference::new(cta1.public_key());
        let ref2 = CtaReference::new(cta1.public_key());
        let ref3 = CtaReference::new(cta2.public_key());

        assert_eq!(ref1, ref2);
        assert_ne!(ref1, ref3);
    }
}
