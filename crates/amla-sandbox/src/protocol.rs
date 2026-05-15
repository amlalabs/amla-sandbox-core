//! Protocol integration layer.
//!
//! This module bridges `amla-protocol` (PCA tokens, signatures) with
//! `amla-capabilities` (typed capability enforcement).
//!
//! # Capability Type Constants
//!
//! These constants define the capability types used in PCA tokens:
//!
//! | Type | Description | Payload Schema |
//! |------|-------------|----------------|
//! | `tool-call` | Call a tool with constraints | `ToolCallCap` |
//! | `memory-read` | Read from memory partition | `MemoryReadCap` |
//! | `memory-write` | Write to memory partition | `MemoryWriteCap` |
//! | `memory-delete` | Delete from memory partition | `MemoryDeleteCap` |
//! | `spawn` | Spawn child sessions | `SpawnCap` |
//!
//! # Trusted Authorities
//!
//! For security, the runtime only accepts PCAs signed by trusted authorities.
//! Use [`set_trusted_authorities`] to configure which public keys are trusted,
//! then [`validate_pca`] will verify both the signature AND that the issuer
//! is in the trusted set.
//!
//! # Example
//!
//! ```rust,ignore
//! use amla_sandbox::protocol::{
//!     capabilities_from_pca, validate_pca, set_trusted_authorities, CAP_TYPE_TOOL_CALL,
//! };
//! use amla_protocol::{Pca, CapabilityData, KeyPair, Algorithm};
//!
//! // For testing: create ephemeral authority and trust it
//! let authority = KeyPair::generate(Algorithm::Ed25519);
//! set_trusted_authorities(vec![authority.public_key()]);
//!
//! // Create and sign a PCA with the authority
//! let pca = PcaBuilder::new()
//!     .add_capability(...)
//!     .build_and_sign(&authority)?;
//!
//! // Validate - checks signature AND issuer is trusted
//! validate_pca(&pca, None, Utc::now())?;
//!
//! // Map capabilities to typed set
//! let cap_set = capabilities_from_pca(&pca)?;
//! ```

use std::cell::RefCell;

use amla_capabilities::{
    CapabilitySet, MemoryDeleteCap, MemoryReadCap, MemoryWriteCap, SpawnCap, ToolCallCap,
};
use amla_protocol::{CapabilityData, Pca, PublicKey};
use chrono::{DateTime, Utc};
use thiserror::Error;

// ============================================================================
// Trusted Authorities
// ============================================================================

thread_local! {
    /// Global set of trusted authority public keys.
    ///
    /// Only PCAs signed by these authorities will be accepted.
    /// Use [`set_trusted_authorities`] to configure.
    static TRUSTED_AUTHORITIES: RefCell<Vec<PublicKey>> = const { RefCell::new(Vec::new()) };
}

/// Set the trusted authorities for PCA validation.
///
/// Only PCAs signed by one of these public keys will be accepted.
/// Call this before creating any runtimes.
///
/// # Example
///
/// ```rust,ignore
/// // For testing: trust an ephemeral test authority
/// let authority = KeyPair::generate(Algorithm::Ed25519);
/// set_trusted_authorities(vec![authority.public_key()]);
///
/// // For production: load from config
/// let prod_keys = load_trusted_keys_from_config();
/// set_trusted_authorities(prod_keys);
/// ```
pub fn set_trusted_authorities(authorities: Vec<PublicKey>) {
    TRUSTED_AUTHORITIES.with(|auth| {
        *auth.borrow_mut() = authorities;
    });
}

/// Get the current trusted authorities.
pub fn get_trusted_authorities() -> Vec<PublicKey> {
    TRUSTED_AUTHORITIES.with(|auth| auth.borrow().clone())
}

/// Clear all trusted authorities.
pub fn clear_trusted_authorities() {
    TRUSTED_AUTHORITIES.with(|auth| {
        auth.borrow_mut().clear();
    });
}

/// Check if an issuer is trusted.
fn is_issuer_trusted(issuer: &PublicKey) -> bool {
    TRUSTED_AUTHORITIES.with(|auth| auth.borrow().iter().any(|a| a == issuer))
}

// ============================================================================
// Capability Type Constants
// ============================================================================

/// Capability type for tool calls.
pub const CAP_TYPE_TOOL_CALL: &str = "tool-call";

/// Capability type for memory reads.
pub const CAP_TYPE_MEMORY_READ: &str = "memory-read";

/// Capability type for memory writes.
pub const CAP_TYPE_MEMORY_WRITE: &str = "memory-write";

/// Capability type for memory deletes.
pub const CAP_TYPE_MEMORY_DELETE: &str = "memory-delete";

/// Capability type for spawning child sessions.
pub const CAP_TYPE_SPAWN: &str = "spawn";

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur during protocol integration.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// PCA signature verification failed.
    #[error("signature verification failed: {0}")]
    SignatureVerificationFailed(String),

    /// PCA has expired.
    #[error("PCA expired at {0}")]
    Expired(String),

    /// Executor mismatch - PCA designates a different executor.
    #[error("executor mismatch: PCA designates {expected}, got {actual}")]
    ExecutorMismatch {
        /// Expected executor (from PCA)
        expected: String,
        /// Actual executor provided
        actual: String,
    },

    /// Unknown capability type in PCA.
    #[error("unknown capability type: {0}")]
    UnknownCapabilityType(String),

    /// Failed to decode capability payload.
    #[error("failed to decode {cap_type} capability: {message}")]
    DecodeError {
        /// Capability type that failed to decode
        cap_type: String,
        /// Error message
        message: String,
    },

    /// PCA has no designated executor (cannot create session).
    #[error("PCA has no designated executor")]
    NoDesignatedExecutor,

    /// PCA issuer is not a trusted authority.
    #[error(
        "issuer not trusted: no trusted authorities configured. Call set_trusted_authorities() before creating runtimes."
    )]
    NoTrustedAuthorities,

    /// PCA issuer is not in the trusted authorities list.
    #[error("issuer not trusted: PCA was signed by an unknown authority")]
    UntrustedIssuer,
}

// ============================================================================
// Capability Mapping
// ============================================================================

/// Extract typed capabilities from a PCA token.
///
/// This function decodes each capability in the PCA and constructs a
/// `CapabilitySet` that can be used for runtime enforcement.
///
/// # Errors
///
/// Returns an error if:
/// - A capability type is unknown
/// - A capability payload fails to decode
///
/// # Example
///
/// ```rust,ignore
/// let pca = Pca::from_cbor(&bytes)?;
/// let cap_set = capabilities_from_pca(&pca)?;
/// ```
pub fn capabilities_from_pca(pca: &Pca) -> Result<CapabilitySet, ProtocolError> {
    let mut cap_set = CapabilitySet::new();

    for cap_data in pca.capabilities() {
        match cap_data.capability_type() {
            CAP_TYPE_TOOL_CALL => {
                let tool_cap: ToolCallCap = decode_capability(cap_data)?;
                cap_set = cap_set.add_tool_call(tool_cap);
            }
            CAP_TYPE_MEMORY_READ => {
                let mem_cap: MemoryReadCap = decode_capability(cap_data)?;
                cap_set = cap_set.add_memory_read(mem_cap);
            }
            CAP_TYPE_MEMORY_WRITE => {
                let mem_cap: MemoryWriteCap = decode_capability(cap_data)?;
                cap_set = cap_set.add_memory_write(mem_cap);
            }
            CAP_TYPE_MEMORY_DELETE => {
                let mem_cap: MemoryDeleteCap = decode_capability(cap_data)?;
                cap_set = cap_set.add_memory_delete(mem_cap);
            }
            CAP_TYPE_SPAWN => {
                let spawn_cap: SpawnCap = decode_capability(cap_data)?;
                cap_set = cap_set.add_spawn(spawn_cap);
            }
            unknown => {
                return Err(ProtocolError::UnknownCapabilityType(unknown.to_string()));
            }
        }
    }

    Ok(cap_set)
}

/// Decode a capability from CBOR bytes.
fn decode_capability<T: serde::de::DeserializeOwned>(
    cap_data: &CapabilityData,
) -> Result<T, ProtocolError> {
    cap_data.decode().map_err(|e| ProtocolError::DecodeError {
        cap_type: cap_data.capability_type().to_string(),
        message: e.to_string(),
    })
}

// ============================================================================
// PCA Validation
// ============================================================================

/// Validate a PCA for session creation.
///
/// This performs all cryptographic and temporal checks required before
/// creating a session from a PCA.
///
/// # Checks Performed
///
/// 1. Trusted authority check (issuer must be in trusted set)
/// 2. Signature verification (cryptographic integrity)
/// 3. Expiry check (PCA must not be expired)
/// 4. Executor binding (if `executor` is provided, must match designated executor)
///
/// # Arguments
///
/// * `pca` - The PCA to validate
/// * `executor` - Optional executor public key to verify against
/// * `current_time` - Time to use for expiry check (for testing)
///
/// # Errors
///
/// Returns an error if any validation check fails.
///
/// # Setup Required
///
/// Before calling this function, you must configure trusted authorities
/// using [`set_trusted_authorities`]. If no trusted authorities are configured,
/// validation will fail with [`ProtocolError::NoTrustedAuthorities`].
///
/// # Example
///
/// ```rust,ignore
/// // Create and trust a test authority
/// let authority = KeyPair::generate(Algorithm::Ed25519);
/// set_trusted_authorities(vec![authority.public_key()]);
///
/// // Create a PCA signed by the authority
/// let pca = PcaBuilder::new()
///     .add_capability(cap)
///     .expires_at(Utc::now() + Duration::hours(1))
///     .build_and_sign(&authority)?;
///
/// // Validate - passes because issuer is trusted
/// validate_pca(&pca, None, Utc::now())?;
/// ```
pub fn validate_pca(
    pca: &Pca,
    executor: Option<&PublicKey>,
    current_time: DateTime<Utc>,
) -> Result<(), ProtocolError> {
    // 1. Check trusted authorities are configured
    let authorities = get_trusted_authorities();
    if authorities.is_empty() {
        return Err(ProtocolError::NoTrustedAuthorities);
    }

    // 2. Check issuer is trusted
    if !is_issuer_trusted(pca.issuer()) {
        return Err(ProtocolError::UntrustedIssuer);
    }

    // 3. Verify signature
    pca.try_verify_signature()
        .map_err(|e| ProtocolError::SignatureVerificationFailed(e.to_string()))?;

    // 4. Check expiry
    if pca.is_expired_at(current_time) {
        return Err(ProtocolError::Expired(pca.expires_at().to_rfc3339()));
    }

    // 5. Check executor binding (if provided)
    if let Some(executor_key) = executor {
        match pca.designated_executor_key() {
            Some(designated) if designated == executor_key => {
                // Match - good
            }
            Some(designated) => {
                return Err(ProtocolError::ExecutorMismatch {
                    expected: format!("{designated:?}"),
                    actual: format!("{executor_key:?}"),
                });
            }
            None => {
                return Err(ProtocolError::NoDesignatedExecutor);
            }
        }
    }

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use amla_constraints::{Constraint, ConstraintSet};
    use amla_protocol::{Algorithm, KeyPair, PcaBuilder, Version};
    use chrono::Duration;
    use serde_json::json;

    fn keypair(seed: u8) -> KeyPair {
        KeyPair::from_seed(Algorithm::Ed25519, &[seed; 32])
    }

    /// Helper to set up trusted authorities and clean up after test.
    struct TrustedAuthoritiesGuard;

    impl TrustedAuthoritiesGuard {
        fn new(authorities: Vec<PublicKey>) -> Self {
            set_trusted_authorities(authorities);
            Self
        }
    }

    impl Drop for TrustedAuthoritiesGuard {
        fn drop(&mut self) {
            clear_trusted_authorities();
        }
    }

    #[test]
    fn test_capabilities_from_pca_tool_call() {
        let issuer = keypair(1);
        let executor = keypair(2);

        // Create a ToolCallCap and serialize it
        let tool_cap = ToolCallCap::with_constraints(
            "stripe:charge",
            ConstraintSet::new(vec![Constraint::Le {
                param: "amount".to_string(),
                value: json!(10000),
            }]),
        );

        let cap_data = CapabilityData::new("cap:charge", CAP_TYPE_TOOL_CALL, &tool_cap).unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        let cap_set = capabilities_from_pca(&pca).unwrap();
        assert_eq!(cap_set.tool_calls.len(), 1);
        assert_eq!(cap_set.tool_calls[0].tool, "stripe:charge");
    }

    #[test]
    fn test_capabilities_from_pca_memory_caps() {
        let issuer = keypair(1);
        let executor = keypair(2);

        let read_cap = MemoryReadCap::new("user/alice/**");
        let write_cap = MemoryWriteCap::with_max_bytes("user/alice/scratch/**", 1024);

        let read_data = CapabilityData::new("cap:read", CAP_TYPE_MEMORY_READ, &read_cap).unwrap();
        let write_data =
            CapabilityData::new("cap:write", CAP_TYPE_MEMORY_WRITE, &write_cap).unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(read_data)
            .add_capability(write_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        let cap_set = capabilities_from_pca(&pca).unwrap();
        assert_eq!(cap_set.memory_reads.len(), 1);
        assert_eq!(cap_set.memory_writes.len(), 1);
    }

    #[test]
    fn test_capabilities_from_pca_unknown_type() {
        let issuer = keypair(1);
        let executor = keypair(2);

        let unknown_data =
            CapabilityData::from_json("cap:unknown", "unknown-type", &json!({})).unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(unknown_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        let result = capabilities_from_pca(&pca);
        assert!(matches!(
            result,
            Err(ProtocolError::UnknownCapabilityType(_))
        ));
    }

    #[test]
    fn test_validate_pca_no_trusted_authorities() {
        // Ensure no trusted authorities are configured
        clear_trusted_authorities();

        let issuer = keypair(1);
        let executor = keypair(2);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        // Should fail because no trusted authorities are configured
        let result = validate_pca(&pca, None, Utc::now());
        assert!(
            matches!(result, Err(ProtocolError::NoTrustedAuthorities)),
            "validate_pca should fail when no trusted authorities are configured"
        );
    }

    #[test]
    fn test_validate_pca_untrusted_issuer() {
        let issuer = keypair(1);
        let executor = keypair(2);
        let other_authority = keypair(99);

        // Trust a different authority, not the issuer
        let _guard = TrustedAuthoritiesGuard::new(vec![other_authority.public_key()]);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        // Should fail because issuer is not trusted
        let result = validate_pca(&pca, None, Utc::now());
        assert!(
            matches!(result, Err(ProtocolError::UntrustedIssuer)),
            "validate_pca should fail when issuer is not trusted"
        );
    }

    #[test]
    fn test_validate_pca_trusted_issuer() {
        let issuer = keypair(1);
        let executor = keypair(2);

        // Trust the issuer
        let _guard = TrustedAuthoritiesGuard::new(vec![issuer.public_key()]);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        // Should succeed because issuer is trusted
        let result = validate_pca(&pca, Some(&executor.public_key()), Utc::now());
        assert!(
            result.is_ok(),
            "validate_pca should succeed for trusted issuer"
        );
    }

    #[test]
    fn test_validate_pca_expired() {
        let issuer = keypair(1);
        let executor = keypair(2);

        // Trust the issuer
        let _guard = TrustedAuthoritiesGuard::new(vec![issuer.public_key()]);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() - Duration::hours(1)) // Already expired
            .build_and_sign(&issuer)
            .unwrap();

        let result = validate_pca(&pca, None, Utc::now());
        assert!(matches!(result, Err(ProtocolError::Expired(_))));
    }

    #[test]
    fn test_validate_pca_executor_mismatch() {
        let issuer = keypair(1);
        let executor = keypair(2);
        let wrong_executor = keypair(10);

        // Trust the issuer
        let _guard = TrustedAuthoritiesGuard::new(vec![issuer.public_key()]);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&issuer)
            .unwrap();

        let result = validate_pca(&pca, Some(&wrong_executor.public_key()), Utc::now());
        assert!(matches!(
            result,
            Err(ProtocolError::ExecutorMismatch { .. })
        ));
    }

    #[test]
    fn test_multiple_trusted_authorities() {
        let authority1 = keypair(1);
        let authority2 = keypair(2);
        let executor = keypair(3);

        // Trust multiple authorities
        let _guard =
            TrustedAuthoritiesGuard::new(vec![authority1.public_key(), authority2.public_key()]);

        let cap_data =
            CapabilityData::from_json("cap:test", CAP_TYPE_TOOL_CALL, &json!({"tool": "test"}))
                .unwrap();

        // PCA signed by authority2 should be accepted
        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap_data)
            .designated_executor(executor.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&authority2)
            .unwrap();

        let result = validate_pca(&pca, Some(&executor.public_key()), Utc::now());
        assert!(
            result.is_ok(),
            "PCA from second trusted authority should be valid"
        );
    }
}
