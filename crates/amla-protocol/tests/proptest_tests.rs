//! Property-based tests for amla-protocol using proptest.
//!
//! These tests verify that our types behave correctly across many random inputs.

// Proptest doesn't support WASM
#![cfg(not(target_arch = "wasm32"))]
#![allow(clippy::unreadable_literal)]

use amla_protocol::{
    Algorithm, CapabilityData, ChainValidationError, KeyPair, PROTOCOL_VERSION, Pca, PcaBuilder,
    PcaHash, PermissiveValidator, PrivateKey, PublicKey, Signature, StrictValidator, Version,
    canonical_cbor_encode, cbor_decode, validate_chain,
};
use chrono::{Duration, Utc};
use proptest::prelude::*;

// Strategies for generating test data

fn arb_version() -> impl Strategy<Value = Version> {
    (0u32..100, 0u32..100).prop_map(|(major, minor)| Version::new(major, minor))
}

fn arb_capability_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("function".to_string()),
        Just("resource".to_string()),
        Just("scope".to_string()),
        "[a-z]{3,10}".prop_map(|s| s),
    ]
}

fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::json!(n)),
        "[a-zA-Z0-9 ]{0,50}".prop_map(serde_json::Value::String),
    ]
}

fn arb_key() -> impl Strategy<Value = String> {
    "[a-z]{3,12}".prop_map(|s| format!("cap:{s}"))
}

fn arb_capability_data() -> impl Strategy<Value = CapabilityData> {
    (arb_key(), arb_capability_type(), arb_json_value()).prop_map(|(key, cap_type, data)| {
        let payload = serde_json::json!({ "data": data });
        CapabilityData::from_json(&key, &cap_type, &payload).unwrap()
    })
}

// Property tests

proptest! {
    /// Signing and verifying should always work for valid identities.
    #[test]
    fn prop_sign_verify_roundtrip(message in prop::collection::vec(any::<u8>(), 0..1000)) {
        let identity = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let signature = identity.sign(&message);
        prop_assert!(identity.verify(&message, &signature).is_ok());
    }

    /// Verification should fail with wrong message.
    #[test]
    fn prop_verify_fails_wrong_message(
        message1 in prop::collection::vec(any::<u8>(), 1..100),
        message2 in prop::collection::vec(any::<u8>(), 1..100),
    ) {
        prop_assume!(message1 != message2);

        let identity = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let signature = identity.sign(&message1);
        prop_assert!(identity.verify(&message2, &signature).is_err());
    }

    /// Verification should fail with wrong public key.
    #[test]
    fn prop_verify_fails_wrong_key(message in prop::collection::vec(any::<u8>(), 0..100)) {
        let identity1 = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let identity2 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let signature = identity1.sign(&message);
        prop_assert!(identity2.verify(&message, &signature).is_err());
    }

    /// Public key hex roundtrip should preserve value.
    #[test]
    fn prop_public_key_hex_roundtrip(_seed in any::<u64>()) {
        let identity = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk = identity.public_key();
        let hex = pk.to_hex();
        let pk2 = PublicKey::from_hex(&hex).unwrap();
        prop_assert_eq!(pk, pk2);
    }

    /// Private key hex roundtrip should preserve value.
    #[test]
    fn prop_private_key_hex_roundtrip(_seed in any::<u64>()) {
        let identity = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sk = identity.private_key();
        let hex = sk.to_hex();
        let sk2 = PrivateKey::from_hex(&hex).unwrap();
        prop_assert_eq!(sk, sk2);
    }

    /// Signature hex roundtrip should preserve value.
    #[test]
    fn prop_signature_hex_roundtrip(message in prop::collection::vec(any::<u8>(), 0..100)) {
        let identity = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig = identity.sign(&message);
        let hex = sig.to_hex();
        let sig2 = Signature::from_hex(&hex).unwrap();
        prop_assert_eq!(sig, sig2);
    }

    /// Hash should be deterministic.
    #[test]
    fn prop_hash_deterministic(data in prop::collection::vec(any::<u8>(), 0..1000)) {
        let hash1 = PcaHash::compute(&data);
        let hash2 = PcaHash::compute(&data);
        prop_assert_eq!(hash1, hash2);
    }

    /// Different data should (almost always) produce different hashes.
    #[test]
    fn prop_hash_collision_resistant(
        data1 in prop::collection::vec(any::<u8>(), 1..100),
        data2 in prop::collection::vec(any::<u8>(), 1..100),
    ) {
        prop_assume!(data1 != data2);
        let hash1 = PcaHash::compute(&data1);
        let hash2 = PcaHash::compute(&data2);
        prop_assert_ne!(hash1, hash2);
    }

    /// Hash hex roundtrip should preserve value.
    #[test]
    fn prop_hash_hex_roundtrip(data in prop::collection::vec(any::<u8>(), 0..100)) {
        let hash = PcaHash::compute(&data);
        let hex = hash.to_hex();
        let hash2 = PcaHash::from_hex(&hex).unwrap();
        prop_assert_eq!(hash, hash2);
    }

    /// Version ordering should be consistent.
    #[test]
    fn prop_version_ordering(major1 in 0u32..100, minor1 in 0u32..100, major2 in 0u32..100, minor2 in 0u32..100) {
        let v1 = Version::new(major1, minor1);
        let v2 = Version::new(major2, minor2);

        if major1 < major2 || (major1 == major2 && minor1 < minor2) {
            prop_assert!(v1 < v2);
        } else if major1 > major2 || (major1 == major2 && minor1 > minor2) {
            prop_assert!(v1 > v2);
        } else {
            prop_assert_eq!(v1, v2);
        }
    }

    /// CapabilityData should roundtrip through CBOR.
    #[test]
    fn prop_capability_data_cbor_roundtrip(cap in arb_capability_data()) {
        // Serialize to CBOR
        let cbor = canonical_cbor_encode(&cap).unwrap();
        // Deserialize back
        let cap2: CapabilityData = cbor_decode(&cbor).unwrap();
        prop_assert_eq!(cap, cap2);
    }

    /// PCA CBOR roundtrip should preserve all fields.
    #[test]
    fn prop_pca_cbor_roundtrip(
        version in arb_version(),
        cap in arb_capability_data(),
        hours_until_expiry in 1i64..1000,
    ) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(hours_until_expiry);

        let pca = PcaBuilder::new()
            .version(version)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let cbor = pca.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor).unwrap();

        prop_assert_eq!(pca.try_hash().unwrap(), pca2.try_hash().unwrap());
        prop_assert!(pca2.try_verify_signature().is_ok());
        prop_assert_eq!(pca.issuer(), pca2.issuer());
        prop_assert_eq!(pca.designated_executor(), pca2.designated_executor());
    }

    /// PCA hash should be deterministic.
    #[test]
    fn prop_pca_hash_deterministic(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca1 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        prop_assert_eq!(pca1.try_hash().unwrap(), pca2.try_hash().unwrap());
    }

    /// PCA signature should always verify.
    #[test]
    fn prop_pca_signature_always_valid(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        prop_assert!(pca.try_verify_signature().is_ok());
        prop_assert_eq!(pca.issuer(), &gateway.public_key());
    }

    /// PCA chain validation should work for valid chains.
    #[test]
    fn prop_valid_chain_validates(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, child];
        let validator = PermissiveValidator;
        let result = amla_protocol::validate_chain(
            &chain,
            &gateway.public_key(),
            &validator,
            Utc::now(),
        );

        prop_assert!(result.is_ok());
    }

    /// CBOR encoding should be deterministic.
    #[test]
    fn prop_cbor_encoding_deterministic(
        s in "[a-zA-Z0-9]{0,50}",
        n in any::<i64>(),
        b in any::<bool>(),
    ) {
        let data = serde_json::json!({
            "string": s,
            "number": n,
            "bool": b
        });

        let enc1 = canonical_cbor_encode(&data).unwrap();
        let enc2 = canonical_cbor_encode(&data).unwrap();

        prop_assert_eq!(enc1, enc2);
    }

    /// CBOR roundtrip should preserve JSON values.
    #[test]
    fn prop_cbor_json_roundtrip(
        s in "[a-zA-Z0-9 ]{0,50}",
        n in -1000000i64..1000000,
        b in any::<bool>(),
    ) {
        let data = serde_json::json!({
            "string": s,
            "number": n,
            "bool": b,
            "null": null
        });

        let encoded = canonical_cbor_encode(&data).unwrap();
        let decoded: serde_json::Value = cbor_decode(&encoded).unwrap();

        prop_assert_eq!(data, decoded);
    }

    // ========================================================================
    // Invalid Input Handling Tests
    // ========================================================================

    /// Invalid hex strings should produce errors, not panics.
    #[test]
    fn prop_invalid_hex_public_key_errors(
        prefix in "[a-z]{0,10}",
        hex_part in "[g-z]{0,70}",  // Invalid hex chars
    ) {
        let invalid_hex = format!("{prefix}:{hex_part}");
        let result = PublicKey::from_hex(&invalid_hex);
        prop_assert!(result.is_err());
    }

    /// Wrong-length hex should produce errors for PublicKey.
    #[test]
    fn prop_wrong_length_public_key_errors(
        len in 0usize..128,
    ) {
        prop_assume!(len != 64);  // 64 hex chars = 32 bytes (valid Ed25519)
        let hex = "a".repeat(len);
        let result = PublicKey::from_hex(&format!("ed25519:{hex}"));
        prop_assert!(result.is_err());
    }

    /// Wrong-length hex should produce errors for PrivateKey.
    #[test]
    fn prop_wrong_length_private_key_errors(
        len in 0usize..128,
    ) {
        prop_assume!(len != 64);  // 64 hex chars = 32 bytes (valid Ed25519)
        let hex = "a".repeat(len);
        let result = PrivateKey::from_hex(&format!("ed25519:{hex}"));
        prop_assert!(result.is_err());
    }

    /// Wrong-length hex should produce errors for Signature.
    #[test]
    fn prop_wrong_length_signature_errors(
        len in 0usize..256,
    ) {
        prop_assume!(len != 128);  // 128 hex chars = 64 bytes (valid Ed25519)
        let hex = "a".repeat(len);
        let result = Signature::from_hex(&format!("ed25519:{hex}"));
        prop_assert!(result.is_err());
    }

    /// Wrong-length hex should produce errors for PcaHash.
    #[test]
    fn prop_wrong_length_hash_errors(
        len in 0usize..128,
    ) {
        prop_assume!(len != 64);  // 64 hex chars = 32 bytes (valid SHA-256)
        let hex = "a".repeat(len);
        let result = PcaHash::from_hex(&hex);
        prop_assert!(result.is_err());
    }

    /// Unknown algorithm prefix should produce errors.
    #[test]
    fn prop_unknown_algorithm_errors(
        algo in "[a-z]{1,10}",
        hex in "[a-f0-9]{64}",
    ) {
        prop_assume!(algo != "ed25519");
        let result = PublicKey::from_hex(&format!("{algo}:{hex}"));
        prop_assert!(result.is_err());
    }

    // ========================================================================
    // Chain Validation Failure Tests
    // ========================================================================

    /// Expired PCAs should always be rejected.
    #[test]
    fn prop_expired_pca_rejected(
        cap in arb_capability_data(),
        hours_expired in 1i64..1000,
    ) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expired = Utc::now() - Duration::hours(hours_expired);

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expired)
            .build_and_sign(&gateway)
            .unwrap();

        let result = validate_chain(
            &[pca],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        let is_expired = matches!(result, Err(ChainValidationError::Expired { .. }));
        prop_assert!(is_expired, "Expected Expired error, got {:?}", result);
    }

    /// Wrong root authority should always be rejected.
    #[test]
    fn prop_wrong_root_authority_rejected(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let wrong_authority = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let result = validate_chain(
            &[pca],
            &wrong_authority.public_key(),  // Wrong!
            &PermissiveValidator,
            Utc::now(),
        );

        let is_invalid_root = matches!(result, Err(ChainValidationError::InvalidRootAuthority { .. }));
        prop_assert!(is_invalid_root, "Expected InvalidRootAuthority error, got {:?}", result);
    }

    /// Unauthorized issuer (wrong signer for child) should always be rejected.
    #[test]
    fn prop_unauthorized_issuer_rejected(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let attacker = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Attacker signs instead of agent1
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&attacker)  // Wrong signer!
            .unwrap();

        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        let is_unauthorized = matches!(result, Err(ChainValidationError::UnauthorizedIssuer { index: 1, .. }));
        prop_assert!(is_unauthorized, "Expected UnauthorizedIssuer error at index 1, got {:?}", result);
    }

    /// Hash mismatch (wrong prev_hash) should always be rejected.
    #[test]
    fn prop_hash_mismatch_rejected(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Wrong prev_hash
        let wrong_hash = PcaHash::compute(b"not the real parent");

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .prev_hash(wrong_hash) // Wrong hash!
            .root_hash(root.try_hash().unwrap())
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        let is_hash_mismatch = matches!(result, Err(ChainValidationError::HashMismatch { index: 1, .. }));
        prop_assert!(is_hash_mismatch, "Expected HashMismatch error at index 1, got {:?}", result);
    }

    /// Missing prev_hash on non-root should always be rejected.
    #[test]
    fn prop_missing_prev_hash_rejected(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Child without prev_hash
        let orphan = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .expires_at(expires)
            .build_and_sign(&agent1)  // No prev_hash!
            .unwrap();

        let result = validate_chain(
            &[root, orphan],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        let is_missing_prev = matches!(result, Err(ChainValidationError::MissingPrevHash { index: 1 }));
        prop_assert!(is_missing_prev, "Expected MissingPrevHash error at index 1, got {:?}", result);
    }

    // ========================================================================
    // Capability Transition Tests
    // ========================================================================

    /// Child capabilities must be subset of parent (unknown keys rejected).
    #[test]
    fn prop_unknown_capability_key_rejected(
        parent_suffix in "[a-z]{3,8}",
        child_suffix in "[a-z]{3,8}",
    ) {
        prop_assume!(parent_suffix != child_suffix);

        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let parent_cap = CapabilityData::from_json(
            &format!("cap:{parent_suffix}"),
            "function",
            &serde_json::json!({"name": "parent"}),
        ).unwrap();

        let child_cap = CapabilityData::from_json(
            &format!("cap:{child_suffix}"),  // Different key!
            "function",
            &serde_json::json!({"name": "child"}),
        ).unwrap();

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(parent_cap)
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(child_cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        let is_unknown_key = matches!(result, Err(ChainValidationError::UnknownCapabilityKey { index: 1, .. }));
        prop_assert!(is_unknown_key, "Expected UnknownCapabilityKey error at index 1, got {:?}", result);
    }

    /// Dropping capabilities should always be allowed (subset rule).
    #[test]
    fn prop_dropping_capabilities_allowed(
        key1 in "[a-z]{3,6}",
        key2 in "[a-z]{3,6}",
    ) {
        prop_assume!(key1 != key2);

        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let cap1 = CapabilityData::from_json(
            &format!("cap:{key1}"),
            "function",
            &serde_json::json!({"name": "cap1"}),
        ).unwrap();

        let cap2 = CapabilityData::from_json(
            &format!("cap:{key2}"),
            "function",
            &serde_json::json!({"name": "cap2"}),
        ).unwrap();

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap1)
            .add_capability(cap2.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Child only keeps cap2, drops cap1
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap2)  // Only one capability
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        prop_assert!(result.is_ok());
    }

    /// StrictValidator should reject any capability change.
    #[test]
    fn prop_strict_validator_rejects_changes(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        // Create a modified capability with same key
        let modified_cap = CapabilityData::from_json(
            cap.key(),
            cap.capability_type(),
            &serde_json::json!({"modified": true}),
        ).unwrap();

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(modified_cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &StrictValidator,  // Rejects any change
            Utc::now(),
        );

        let is_invalid_transition = matches!(result, Err(ChainValidationError::InvalidTransition { index: 1, .. }));
        prop_assert!(is_invalid_transition, "Expected InvalidTransition error at index 1, got {:?}", result);
    }

    // ========================================================================
    // Wire Format Stability Tests
    // ========================================================================

    /// PCA CBOR should be deterministic regardless of build order.
    #[test]
    fn prop_pca_cbor_deterministic(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca1 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let cbor1 = pca1.to_cbor().unwrap();
        let cbor2 = pca2.to_cbor().unwrap();

        prop_assert_eq!(cbor1, cbor2);
    }

    /// PCA should survive multiple roundtrips.
    #[test]
    fn prop_pca_multi_roundtrip(cap in arb_capability_data()) {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let original = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Multiple roundtrips
        let cbor1 = original.to_cbor().unwrap();
        let pca1 = Pca::from_cbor(&cbor1).unwrap();
        let cbor2 = pca1.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor2).unwrap();
        let cbor3 = pca2.to_cbor().unwrap();

        // All CBORs should be identical
        prop_assert_eq!(&cbor1, &cbor2);
        prop_assert_eq!(&cbor2, &cbor3);

        // Final should still verify
        prop_assert!(pca2.try_verify_signature().is_ok());
        prop_assert_eq!(original.try_hash().unwrap(), pca2.try_hash().unwrap());
    }

    /// CapabilityData wire format should be stable across roundtrips.
    #[test]
    fn prop_capability_wire_stable(cap in arb_capability_data()) {
        let cbor1 = canonical_cbor_encode(&cap).unwrap();
        let cap2: CapabilityData = cbor_decode(&cbor1).unwrap();
        let cbor2 = canonical_cbor_encode(&cap2).unwrap();

        prop_assert_eq!(cbor1, cbor2);
        prop_assert_eq!(cap, cap2);
    }

    /// PublicKey wire format should be stable.
    #[test]
    fn prop_public_key_wire_stable(_seed in any::<u64>()) {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk = keypair.public_key();

        let cbor1 = canonical_cbor_encode(&pk).unwrap();
        let pk2: PublicKey = cbor_decode(&cbor1).unwrap();
        let cbor2 = canonical_cbor_encode(&pk2).unwrap();

        prop_assert_eq!(cbor1, cbor2);
        prop_assert_eq!(pk, pk2);
    }

    /// Signature wire format should be stable.
    #[test]
    fn prop_signature_wire_stable(msg in prop::collection::vec(any::<u8>(), 0..100)) {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig = keypair.sign(&msg);

        let cbor1 = canonical_cbor_encode(&sig).unwrap();
        let sig2: Signature = cbor_decode(&cbor1).unwrap();
        let cbor2 = canonical_cbor_encode(&sig2).unwrap();

        prop_assert_eq!(cbor1, cbor2);
        prop_assert_eq!(sig, sig2);
    }
}
