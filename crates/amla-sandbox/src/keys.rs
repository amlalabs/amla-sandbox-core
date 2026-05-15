//! Key management for multi-tenant runtime isolation.
//!
//! This module provides a key registry for storing Ed25519 keypairs by ID,
//! enabling multi-tenant WASM deployments where each tenant has its own
//! cryptographic identity.
//!
//! # Design
//!
//! The key registry is thread-local (like the runtime registry) to support
//! WASM's single-threaded execution model. Each key is identified by a
//! string ID chosen by the caller.
//!
//! # Security
//!
//! - Private keys are stored in memory and never exposed via WASM
//! - Only public keys can be retrieved through the API
//! - Keys are `Zeroize` on drop (from amla-protocol)
//! - The key registry is separate from runtimes for flexibility
//!
//! # Usage Pattern
//!
//! ```text
//! # Native builds:
//! 1. key_generate("agent-1") or key_set("agent-1", private_key_hex)
//!
//! # WASM builds (deterministic):
//! 1. key_generate_from_seed("agent-1", host_provided_seed)
//!
//! # Both:
//! 2. runtime_new_with_key("agent-1", pca_bytes)  // Validates PCA executor
//! 3. key_sign("agent-1", data)  // Sign data with the key
//! ```

use std::cell::RefCell;
use std::collections::HashMap;

use amla_protocol::{Algorithm, KeyPair, PublicKey, Signature};
use thiserror::Error;

// =============================================================================
// Error Types
// =============================================================================

/// Errors that can occur during key operations.
#[derive(Debug, Error)]
pub enum KeyError {
    /// Key not found in registry.
    #[error("key not found: {0}")]
    NotFound(String),

    /// Key already exists (for set operations).
    #[error("key already exists: {0}")]
    AlreadyExists(String),

    /// Invalid key format.
    #[error("invalid key format: {0}")]
    InvalidFormat(String),

    /// Signature verification failed.
    #[error("signature verification failed: {0}")]
    VerificationFailed(String),
}

// =============================================================================
// Key Registry (Thread-Local)
// =============================================================================

thread_local! {
    /// Registry of all keypairs, keyed by string ID.
    ///
    /// This is thread-local like RUNTIMES for WASM compatibility.
    /// In a multi-tenant deployment, each tenant would use a unique key ID.
    static KEYS: RefCell<HashMap<String, KeyPair>> = RefCell::new(HashMap::new());
}

// =============================================================================
// Public API
// =============================================================================

/// Generate an Ed25519 keypair from a seed and store it in the registry.
///
/// The seed must be 32 bytes of cryptographically random data, typically
/// provided by the host in WASM environments.
///
/// # Arguments
///
/// * `key_id` - Unique identifier for this key
/// * `seed` - 32 bytes of cryptographically random data
///
/// # Returns
///
/// The public key in hex format (e.g., "ed25519:abc123...")
///
/// # Errors
///
/// Returns `KeyError::AlreadyExists` if a key with this ID already exists.
pub fn key_generate_from_seed(key_id: &str, seed: &[u8; 32]) -> Result<String, KeyError> {
    KEYS.with(|keys| {
        let mut keys = keys.borrow_mut();

        if keys.contains_key(key_id) {
            return Err(KeyError::AlreadyExists(key_id.to_string()));
        }

        let keypair = KeyPair::from_seed(Algorithm::Ed25519, seed);
        let public_hex = keypair.public_key().to_hex();

        keys.insert(key_id.to_string(), keypair);

        Ok(public_hex)
    })
}

/// Import an existing keypair from a private key.
///
/// # Arguments
///
/// * `key_id` - Unique identifier for this key
/// * `private_key_hex` - Private key in hex format (e.g., "ed25519:abc123...")
///
/// # Returns
///
/// The public key in hex format
///
/// # Errors
///
/// - `KeyError::AlreadyExists` if a key with this ID already exists
/// - `KeyError::InvalidFormat` if the private key cannot be parsed
pub fn key_set(key_id: &str, private_key_hex: &str) -> Result<String, KeyError> {
    use amla_protocol::PrivateKey;

    KEYS.with(|keys| {
        let mut keys = keys.borrow_mut();

        if keys.contains_key(key_id) {
            return Err(KeyError::AlreadyExists(key_id.to_string()));
        }

        let private_key = PrivateKey::from_hex(private_key_hex)
            .map_err(|e| KeyError::InvalidFormat(e.to_string()))?;

        let keypair = KeyPair::from_private_key(&private_key)
            .map_err(|e| KeyError::InvalidFormat(e.to_string()))?;

        let public_hex = keypair.public_key().to_hex();

        keys.insert(key_id.to_string(), keypair);

        Ok(public_hex)
    })
}

/// Get the public key for a stored keypair.
///
/// # Arguments
///
/// * `key_id` - Key identifier
///
/// # Returns
///
/// Public key in hex format
///
/// # Errors
///
/// Returns `KeyError::NotFound` if no key exists with this ID.
pub fn key_get_public(key_id: &str) -> Result<String, KeyError> {
    KEYS.with(|keys| {
        let keys = keys.borrow();

        keys.get(key_id)
            .map(|kp| kp.public_key().to_hex())
            .ok_or_else(|| KeyError::NotFound(key_id.to_string()))
    })
}

/// Get the public key object for a stored keypair.
///
/// This is for internal use (e.g., PCA validation).
pub fn key_get_public_key(key_id: &str) -> Result<PublicKey, KeyError> {
    KEYS.with(|keys| {
        let keys = keys.borrow();

        keys.get(key_id)
            .map(amla_protocol::KeyPair::public_key)
            .ok_or_else(|| KeyError::NotFound(key_id.to_string()))
    })
}

/// Get a clone of the full keypair for internal signing operations.
///
/// This is for internal use only (e.g., PCA creation). The keypair is cloned
/// to avoid holding the borrow across external calls.
///
/// # Security
///
/// This function is NOT exposed via WASM exports. Only internal Rust code
/// can access keypairs for signing operations.
pub fn key_get_keypair(key_id: &str) -> Result<KeyPair, KeyError> {
    KEYS.with(|keys| {
        let keys = keys.borrow();

        keys.get(key_id)
            .cloned()
            .ok_or_else(|| KeyError::NotFound(key_id.to_string()))
    })
}

/// Sign data with a stored keypair.
///
/// # Arguments
///
/// * `key_id` - Key identifier
/// * `data` - Data to sign
///
/// # Returns
///
/// Signature in hex format (e.g., "ed25519:abc123...")
///
/// # Errors
///
/// Returns `KeyError::NotFound` if no key exists with this ID.
pub fn key_sign(key_id: &str, data: &[u8]) -> Result<String, KeyError> {
    KEYS.with(|keys| {
        let keys = keys.borrow();

        keys.get(key_id)
            .map(|kp| kp.sign(data).to_hex())
            .ok_or_else(|| KeyError::NotFound(key_id.to_string()))
    })
}

/// Verify a signature against a public key.
///
/// This does NOT require the key to be in the registry - it uses the
/// provided public key directly.
///
/// # Arguments
///
/// * `public_key_hex` - Public key in hex format
/// * `data` - Original data that was signed
/// * `signature_hex` - Signature to verify in hex format
///
/// # Returns
///
/// `Ok(())` if signature is valid
///
/// # Errors
///
/// - `KeyError::InvalidFormat` if public key or signature cannot be parsed
/// - `KeyError::VerificationFailed` if signature is invalid
pub fn key_verify(public_key_hex: &str, data: &[u8], signature_hex: &str) -> Result<(), KeyError> {
    let public_key =
        PublicKey::from_hex(public_key_hex).map_err(|e| KeyError::InvalidFormat(e.to_string()))?;

    let signature =
        Signature::from_hex(signature_hex).map_err(|e| KeyError::InvalidFormat(e.to_string()))?;

    public_key
        .verify(data, &signature)
        .map_err(|e| KeyError::VerificationFailed(e.to_string()))
}

/// Delete a key from the registry.
///
/// # Arguments
///
/// * `key_id` - Key identifier
///
/// # Returns
///
/// `true` if a key was removed, `false` if no key existed with this ID.
pub fn key_delete(key_id: &str) -> bool {
    KEYS.with(|keys| keys.borrow_mut().remove(key_id).is_some())
}

/// Check if a key exists in the registry.
pub fn key_exists(key_id: &str) -> bool {
    KEYS.with(|keys| keys.borrow().contains_key(key_id))
}

/// Get count of keys in registry (for testing).
pub fn key_count() -> usize {
    KEYS.with(|keys| keys.borrow().len())
}

/// Clear all keys (for testing).
#[cfg(test)]
pub fn clear_keys() {
    KEYS.with(|keys| keys.borrow_mut().clear());
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // Test seeds (deterministic)
    const TEST_SEED_1: [u8; 32] = [1u8; 32];
    const TEST_SEED_2: [u8; 32] = [2u8; 32];

    fn setup() {
        clear_keys();
    }

    #[test]
    fn test_key_generate_from_seed() {
        setup();

        let public_hex = key_generate_from_seed("test-key", &TEST_SEED_1).unwrap();
        assert!(public_hex.starts_with("ed25519:"));
        assert!(key_exists("test-key"));

        // Duplicate should fail
        assert!(matches!(
            key_generate_from_seed("test-key", &TEST_SEED_2),
            Err(KeyError::AlreadyExists(_))
        ));
    }

    #[test]
    fn test_key_generate_from_seed_deterministic() {
        setup();

        // Same seed should produce same key
        let public1 = key_generate_from_seed("key1", &TEST_SEED_1).unwrap();
        key_delete("key1");

        let public2 = key_generate_from_seed("key1", &TEST_SEED_1).unwrap();
        assert_eq!(public1, public2);

        // Different seed should produce different key
        key_delete("key1");
        let public3 = key_generate_from_seed("key1", &TEST_SEED_2).unwrap();
        assert_ne!(public1, public3);
    }

    #[test]
    fn test_key_set() {
        setup();

        // Create a key from seed to get valid format
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &TEST_SEED_1);
        let private_hex = keypair.private_key().to_hex();
        let expected_public = keypair.public_key().to_hex();

        let public_hex = key_set("imported-key", &private_hex).unwrap();
        assert_eq!(public_hex, expected_public);
        assert!(key_exists("imported-key"));
    }

    #[test]
    fn test_key_set_invalid() {
        setup();

        assert!(matches!(
            key_set("bad-key", "not-a-valid-key"),
            Err(KeyError::InvalidFormat(_))
        ));
    }

    #[test]
    fn test_key_get_public() {
        setup();

        let generated = key_generate_from_seed("my-key", &TEST_SEED_1).unwrap();
        let retrieved = key_get_public("my-key").unwrap();
        assert_eq!(generated, retrieved);

        assert!(matches!(
            key_get_public("nonexistent"),
            Err(KeyError::NotFound(_))
        ));
    }

    #[test]
    fn test_key_sign_and_verify() {
        setup();

        let public_hex = key_generate_from_seed("signer", &TEST_SEED_1).unwrap();
        let data = b"hello world";

        let signature_hex = key_sign("signer", data).unwrap();
        assert!(signature_hex.starts_with("ed25519:"));

        // Verify with correct data
        assert!(key_verify(&public_hex, data, &signature_hex).is_ok());

        // Verify with wrong data should fail
        assert!(matches!(
            key_verify(&public_hex, b"wrong data", &signature_hex),
            Err(KeyError::VerificationFailed(_))
        ));
    }

    #[test]
    fn test_key_sign_not_found() {
        setup();

        assert!(matches!(
            key_sign("nonexistent", b"data"),
            Err(KeyError::NotFound(_))
        ));
    }

    #[test]
    fn test_key_delete() {
        setup();

        key_generate_from_seed("to-delete", &TEST_SEED_1).unwrap();
        assert!(key_exists("to-delete"));

        assert!(key_delete("to-delete"));
        assert!(!key_exists("to-delete"));

        // Delete non-existent returns false
        assert!(!key_delete("nonexistent"));
    }

    #[test]
    fn test_key_count() {
        setup();

        assert_eq!(key_count(), 0);
        key_generate_from_seed("key1", &TEST_SEED_1).unwrap();
        assert_eq!(key_count(), 1);
        key_generate_from_seed("key2", &TEST_SEED_2).unwrap();
        assert_eq!(key_count(), 2);
    }
}
