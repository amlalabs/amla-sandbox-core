// Allow ref_option_ref in derive macros - unavoidable with serde's Serialize on Option<&T>
#![allow(clippy::ref_option_ref)]
#![forbid(unsafe_code)]

//! Amla Protocol - Core types for capability-based authorization.
//!
//! This crate provides the fundamental types and cryptographic primitives
//! for the Amla authorization protocol, based on the PIC (Provenance Identity
//! Continuity) model.
//!
//! # Design Philosophy
//!
//! The protocol layer is **minimal and generic**:
//!
//! - **Capabilities are opaque**: The protocol treats capabilities as typed
//!   blobs of data. What they mean and how attenuation works is defined by
//!   the application layer.
//!
//! - **Transition validation is pluggable**: Applications implement
//!   [`TransitionValidator`] to define their capability semantics.
//!
//! - **Protocol handles structure**: Chain linking, signatures, expiry.
//!
//! # Quick Start
//!
//! ```rust
//! use amla_protocol::{KeyPair, Algorithm, PcaBuilder, CapabilityData, Version};
//! use chrono::{Utc, Duration};
//! use serde_json::json;
//!
//! // Create keypairs
//! let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
//! let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
//!
//! // Define a capability (key, type, data as CBOR bytes)
//! let cap = CapabilityData::from_json(
//!     "cap:refunds",  // Stable key for matching
//!     "function",
//!     &json!({
//!         "name": "payments.refund",
//!         "constraints": [{"param": "amount", "op": "<=", "value": 500}]
//!     }),
//! ).unwrap();
//!
//! // Create a PCA (signed authorization)
//! let pca = PcaBuilder::new()
//!     .version(Version::new(0, 1))
//!     .add_capability(cap)
//!     .designated_executor(agent.public_key())
//!     .expires_at(Utc::now() + Duration::hours(1))
//!     .build_and_sign(&gateway)
//!     .unwrap();
//!
//! // Verify the signature
//! assert!(pca.try_verify_signature().is_ok());
//!
//! // Get the unique hash (for chain linking)
//! let pca_hash = pca.try_hash().unwrap();
//! println!("PCA ID: {}", pca_hash.to_hex());
//! ```
//!
//! # Pluggable Capability Validation
//!
//! The protocol doesn't interpret capability contents. Applications define
//! their own validation logic:
//!
//! ```rust
//! use amla_protocol::{CapabilityData, TransitionValidator, TransitionError};
//!
//! struct MyValidator;
//!
//! impl TransitionValidator for MyValidator {
//!     fn validate_transition(
//!         &self,
//!         parent: &CapabilityData,
//!         child: &CapabilityData,
//!     ) -> std::result::Result<(), TransitionError> {
//!         // Type must match
//!         if parent.capability_type() != child.capability_type() {
//!             return Err(TransitionError::new("type mismatch"));
//!         }
//!
//!         // Application-specific attenuation logic...
//!         // e.g., check that child.data["max_amount"] <= parent.data["max_amount"]
//!
//!         Ok(())
//!     }
//! }
//! ```
//!
//! # Capability Matching
//!
//! Chain validation uses keyed matching: capabilities are matched by their stable key, allowing
//!   children to reorder or drop capabilities while still enforcing attenuation.
//!
//! ```rust
//! use amla_protocol::{
//!     Algorithm, CapabilityData, KeyPair, PcaBuilder, PermissiveValidator,
//!     PROTOCOL_VERSION, validate_chain,
//! };
//! use chrono::{Duration, Utc};
//!
//! let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
//! let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
//! let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
//!
//! let expires = Utc::now() + Duration::hours(1);
//!
//! let cap_a = CapabilityData::from_json(
//!     "cap:claims",
//!     "function",
//!     &serde_json::json!({"name": "claims.process"}),
//! ).unwrap();
//! let cap_b = CapabilityData::from_json(
//!     "cap:payout",
//!     "function",
//!     &serde_json::json!({"name": "payout.execute"}),
//! ).unwrap();
//!
//! let root = PcaBuilder::new()
//!     .version(PROTOCOL_VERSION)
//!     .add_capability(cap_a.clone())
//!     .add_capability(cap_b.clone())
//!     .designated_executor(agent1.public_key())
//!     .expires_at(expires)
//!     .build_and_sign(&gateway)
//!     .unwrap();
//!
//! // Child drops cap_a but keeps cap_b (allowed with keyed matching)
//! let child = PcaBuilder::new()
//!     .version(PROTOCOL_VERSION)
//!     .add_capability(cap_b)
//!     .designated_executor(agent2.public_key())
//!     .parent_pca(&root)
//!     .unwrap()
//!     .expires_at(expires)  // Must be <= parent expiry
//!     .build_and_sign(&agent1)
//!     .unwrap();
//!
//! let validator = PermissiveValidator;
//! validate_chain(
//!     &[root, child],
//!     &gateway.public_key(),
//!     &validator,
//!     Utc::now(),
//! ).unwrap();
//! ```
//!
//! # Security Properties
//!
//! The protocol provides cryptographic guarantees:
//!
//! 1. **Origin Immutability**: Requests descend from a specific root authority
//! 2. **Chain Integrity**: Each PCA commits to its parent via hash
//! 3. **Executor Binding**: Only the designated executor can use a PCA
//! 4. **Temporal Validity**: PCAs expire at a specified time
//!
//! Capability semantics (what "attenuation" means) are NOT enforced by the
//! protocol - that's the application's job via [`TransitionValidator`].
//!
//! # Wasm Support
//!
//! This crate can be compiled to WebAssembly with the `wasm` feature:
//!
//! ```toml
//! [dependencies]
//! amla-protocol = { version = "0.1", features = ["wasm"] }
//! ```

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod capability;
mod chain;
mod cta;
mod error;
mod executor;
mod hash;
mod identity;
mod pca;
mod serialization;
mod version;

#[cfg(feature = "wasm")]
mod wasm;

// Re-export public API

// Capability types (opaque + validation)
pub use capability::{
    CapabilityData, ClosureValidator, MAX_CAPABILITY_PAYLOAD_SIZE, PermissiveValidator,
    StrictValidator, TransitionError, TransitionValidator, TypeDispatchValidator,
    find_duplicate_capability_key,
};

// Chain validation
pub use chain::{
    ChainValidationError, MAX_CHAIN_DEPTH, get_effective_capabilities, get_final_executor,
    validate_chain, validate_cta_chain,
};

// Error types
pub use error::{Error, Result};

// Executor types
pub use executor::{CtaReference, DesignatedExecutor, ExecutorCharacteristic};

// CTA types
pub use cta::{
    ContinuationRequest, Cta, CtaBuilder, CtaError, CtaSubmission, ExecutorResolver,
    FreshnessChallenge, FreshnessError, FreshnessValidator, POC_DOMAIN_SEPARATOR,
    PermissiveFreshnessValidator, PermissiveResolver, ProofOfContinuity, RejectAllResolver,
    StatefulFreshnessValidator, StatelessFreshnessValidator,
};

// Hash type
pub use hash::PcaHash;

// Identity types
pub use identity::{Algorithm, KeyPair, PrivateKey, PublicKey, Signature};

// PCA types
pub use pca::{Pca, PcaBuilder};

// Serialization utilities
pub use serialization::{
    canonical_cbor_encode, cbor_decode, cbor_decode_to_json, cbor_to_json_value,
    json_to_cbor_value, validate_cbor_serializable,
};

// Version
pub use version::{PROTOCOL_VERSION, Version};

// Wasm bindings (when enabled)
#[cfg(feature = "wasm")]
pub use wasm::{JsCapabilityData, JsIdentity, JsPca, JsPcaBuilder, JsPublicKey, protocol_version};

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
mod integration_tests {
    use super::*;
    use chrono::{Duration, Utc};

    /// Test a complete multi-hop authorization chain (similar to insurance demo).
    #[test]
    fn test_insurance_claim_chain() {
        // Setup keypairs
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let claims_agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let payout_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

        let expires = Utc::now() + Duration::hours(1);

        // Root PCA: Gateway -> Claims Agent
        // "Process claims up to $25,000"
        let claim_cap = CapabilityData::from_json(
            "cap:claims",
            "function",
            &serde_json::json!({
                "name": "insurance.process_claim",
                "max_amount": 2500000,  // $25,000 in cents
                "claim_type": "auto"
            }),
        )
        .unwrap();

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(claim_cap)
            .designated_executor(claims_agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        assert!(root_pca.is_root());
        assert!(root_pca.try_verify_signature().is_ok());

        // Claims Agent attenuates and delegates to Payout Agent
        // "Pay out $3,800 for this specific claim"
        let payout_cap = CapabilityData::from_json(
            "cap:claims",
            "function",
            &serde_json::json!({
                "name": "insurance.execute_payout",
                "claim_id": "CLM-2025-847291",
                "approved_amount": 380000,  // $3,800 in cents
                "deductible": 50000          // $500 deductible
            }),
        )
        .unwrap();

        let child_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(payout_cap)
            .designated_executor(payout_agent.public_key())
            .parent_pca(&root_pca)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&claims_agent)
            .unwrap();

        assert!(!child_pca.is_root());
        assert_eq!(child_pca.prev_hash(), Some(&root_pca.try_hash().unwrap()));
        assert!(child_pca.try_verify_signature().is_ok());
        assert_eq!(child_pca.issuer(), &claims_agent.public_key());
        assert_eq!(
            child_pca.designated_executor().as_public_key(),
            Some(&payout_agent.public_key())
        );

        // Verify chain integrity
        // An attacker (Eve) cannot:
        // 1. Modify the chain (signature verification fails)
        // 2. Extend the chain (not the designated executor)
        // 3. Use the PCA (not the designated executor)

        let attacker = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);

        // Attacker tries to extend the chain
        let attacker_cap = CapabilityData::from_json(
            "cap:claims",
            "function",
            &serde_json::json!({"name": "insurance.steal_money"}),
        )
        .unwrap();

        let attacker_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(attacker_cap)
            .designated_executor(attacker.public_key())
            .parent_pca(&child_pca)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&attacker) // Signed by attacker, not payout_agent
            .unwrap();

        // The attacker's PCA is technically valid (well-formed, properly signed)
        // BUT: the issuer is not the designated_executor of the parent PCA
        // This check would be done by the Gateway/Trust Plane, not the protocol
        assert!(attacker_pca.try_verify_signature().is_ok()); // Signature is valid
        assert_ne!(attacker_pca.issuer(), &payout_agent.public_key()); // But issuer is wrong
    }

    /// Test serialization roundtrip.
    #[test]
    fn test_cbor_roundtrip() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let cap = CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "test.function", "x": 42}),
        )
        .unwrap();

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        // Serialize
        let cbor = pca.to_cbor().unwrap();

        // Deserialize
        let pca2 = Pca::from_cbor(&cbor).unwrap();

        // Verify
        assert_eq!(pca.try_hash().unwrap(), pca2.try_hash().unwrap());
        assert!(pca2.try_verify_signature().is_ok());
        assert_eq!(pca.designated_executor(), pca2.designated_executor());
    }

    /// Test transition validators.
    #[test]
    fn test_transition_validation() {
        let parent = CapabilityData::from_json(
            "cap:api",
            "function",
            &serde_json::json!({"name": "api.call", "max_amount": 1000}),
        )
        .unwrap();

        let valid_child = CapabilityData::from_json(
            "cap:api",
            "function",
            &serde_json::json!({"name": "api.call", "max_amount": 500}),
        )
        .unwrap();

        let invalid_child = CapabilityData::from_json(
            "cap:resource",
            "resource", // Different type!
            &serde_json::json!({"path": "/api"}),
        )
        .unwrap();

        // Permissive validator accepts everything
        let permissive = PermissiveValidator;
        assert!(
            permissive
                .validate_transition(&parent, &valid_child)
                .is_ok()
        );
        assert!(
            permissive
                .validate_transition(&parent, &invalid_child)
                .is_ok()
        );

        // Strict validator only accepts identical
        let strict = StrictValidator;
        assert!(strict.validate_transition(&parent, &parent.clone()).is_ok());
        assert!(strict.validate_transition(&parent, &valid_child).is_err());

        // Custom validator
        let custom = ClosureValidator::new(|p, c| {
            if p.capability_type() != c.capability_type() {
                return Err(TransitionError::new("type mismatch"));
            }
            Ok(())
        });
        assert!(custom.validate_transition(&parent, &valid_child).is_ok());
        assert!(custom.validate_transition(&parent, &invalid_child).is_err());
    }
}
