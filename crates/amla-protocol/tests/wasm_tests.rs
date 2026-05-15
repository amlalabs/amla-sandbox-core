//! WASM-specific tests using wasm-bindgen-test.
//!
//! These tests run in a browser or Node.js environment via wasm-pack.
//! Run with: `wasm-pack test --headless --chrome` or `wasm-pack test --node`

#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

use amla_protocol::{
    Algorithm, CapabilityData, KeyPair, PcaBuilder, PcaHash, PermissiveValidator, Version,
    validate_chain,
};
use chrono::{Duration, Utc};

#[wasm_bindgen_test]
fn test_keypair_generation_wasm() {
    let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    assert_eq!(keypair.algorithm(), Algorithm::Ed25519);

    // Verify we can sign and verify
    let message = b"test message";
    let signature = keypair.sign(message);
    assert!(keypair.verify(message, &signature).is_ok());
}

#[wasm_bindgen_test]
fn test_hash_computation_wasm() {
    let hash1 = PcaHash::compute(b"hello world");
    let hash2 = PcaHash::compute(b"hello world");
    assert_eq!(hash1, hash2);

    let hash3 = PcaHash::compute(b"different");
    assert_ne!(hash1, hash3);
}

#[wasm_bindgen_test]
fn test_pca_creation_wasm() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let cap = CapabilityData::from_json(
        "cap:test",
        "function",
        &serde_json::json!({"name": "test.function"}),
    )
    .unwrap();

    let pca = PcaBuilder::new()
        .version(Version::new(0, 1))
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(Utc::now() + Duration::hours(1))
        .build_and_sign(&gateway)
        .unwrap();

    assert!(pca.try_verify_signature().is_ok());
    assert!(pca.is_root());
}

#[wasm_bindgen_test]
fn test_pca_cbor_roundtrip_wasm() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

    let cap = CapabilityData::from_json(
        "cap:roundtrip",
        "function",
        &serde_json::json!({"value": 42}),
    )
    .unwrap();

    let pca = PcaBuilder::new()
        .version(Version::new(0, 1))
        .add_capability(cap)
        .designated_executor(agent.public_key())
        .expires_at(Utc::now() + Duration::hours(1))
        .build_and_sign(&gateway)
        .unwrap();

    let cbor = pca.to_cbor().unwrap();
    let pca2 = amla_protocol::Pca::from_cbor(&cbor).unwrap();

    assert_eq!(pca.try_hash().unwrap(), pca2.try_hash().unwrap());
    assert!(pca2.try_verify_signature().is_ok());
}

#[wasm_bindgen_test]
fn test_chain_validation_wasm() {
    let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
    let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

    let cap = CapabilityData::from_json(
        "cap:chain",
        "function",
        &serde_json::json!({"name": "chain.test"}),
    )
    .unwrap();

    let expires = Utc::now() + Duration::hours(1);

    let root = PcaBuilder::new()
        .version(Version::new(0, 1))
        .add_capability(cap.clone())
        .designated_executor(agent1.public_key())
        .expires_at(expires)
        .build_and_sign(&gateway)
        .unwrap();

    let child = PcaBuilder::new()
        .version(Version::new(0, 1))
        .add_capability(cap)
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

    assert!(result.is_ok());
}

#[wasm_bindgen_test]
fn test_hex_encoding_wasm() {
    let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

    // Public key hex roundtrip
    let pk_hex = keypair.public_key().to_hex();
    assert!(pk_hex.starts_with("ed25519:"));
    let pk2 = amla_protocol::PublicKey::from_hex(&pk_hex).unwrap();
    assert_eq!(keypair.public_key(), pk2);

    // Signature hex roundtrip
    let sig = keypair.sign(b"test");
    let sig_hex = sig.to_hex();
    let sig2 = amla_protocol::Signature::from_hex(&sig_hex).unwrap();
    assert_eq!(sig, sig2);
}
