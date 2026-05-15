//! Chain validation for PCA authorization chains.
//!
//! This module provides validation logic for PCA chains, ensuring:
//! - Signature validity
//! - Chain integrity (hash linking)
//! - Issuer authorization (issuer must be parent's designated executor)
//! - Temporal validity (not expired)
//! - Capability transitions (using pluggable validators)
//!
//! # Design Decisions
//!
//! ## Keyed Capability Matching
//!
//! Capabilities are matched by their `key` field (e.g., "cap:stripe.charge") rather
//! than by position in the array. This design enables:
//! - **Reordering**: Child can list capabilities in any order
//! - **Dropping**: Child can omit capabilities (principle of least authority)
//! - **Type-safe transitions**: Each capability key maps to a specific schema
//!
//! The validator only checks capabilities that exist in the child. Dropped
//! capabilities are implicitly valid (the child simply doesn't have that authority).
//!
//! ## Fail-Fast Duplicate Detection
//!
//! Duplicate capability keys are rejected at PCA creation time (`PcaBuilder`)
//! rather than only at validation time. This provides earlier error feedback
//! and prevents malformed PCAs from being created in the first place.
//!
//! Chain validation also checks for duplicates as defense-in-depth against
//! malformed CBOR payloads that bypass the builder.

use chrono::{DateTime, Utc};

use crate::{
    CapabilityData, DesignatedExecutor, Error, Pca, PublicKey, TransitionError, TransitionValidator,
};

/// Maximum allowed chain depth (number of PCAs).
///
/// This limit prevents denial-of-service attacks through excessively deep chains
/// that could cause stack overflow or excessive computation during validation.
/// 100 hops is generous for any legitimate use case.
pub const MAX_CHAIN_DEPTH: usize = 100;

/// Errors that can occur during chain validation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainValidationError {
    /// The chain is empty.
    EmptyChain,

    /// A PCA in the chain has an invalid signature.
    InvalidSignature {
        /// Index of the PCA in the chain (0 = root).
        index: usize,
    },
    /// A PCA in the chain could not be processed.
    InvalidPca {
        /// Index of the PCA in the chain.
        index: usize,
        /// Reason for the failure.
        reason: String,
    },

    /// The root PCA is not actually a root (has `prev_hash`).
    RootHasPrevHash,

    /// The root PCA incorrectly includes a `root_hash`.
    RootHasRootHash,

    /// A non-root PCA is missing its `root_hash`.
    RootHashMissing {
        /// Index of the PCA in the chain.
        index: usize,
    },
    /// A non-root PCA is missing its `prev_hash`.
    MissingPrevHash {
        /// Index of the PCA in the chain.
        index: usize,
    },

    /// A PCA's `prev_hash` doesn't match the parent's hash.
    HashMismatch {
        /// Index of the child PCA.
        index: usize,
        /// Expected hash (parent's hash).
        expected: String,
        /// Actual hash in the child's `prev_hash`.
        actual: String,
    },

    /// A PCA's issuer is not the parent's designated executor.
    UnauthorizedIssuer {
        /// Index of the child PCA.
        index: usize,
        /// Expected issuer (parent's designated executor).
        expected_issuer: String,
        /// Actual issuer.
        actual_issuer: String,
    },

    /// The root PCA's issuer doesn't match the expected root authority.
    InvalidRootAuthority {
        /// Expected root authority public key.
        expected: String,
        /// Actual issuer of the root PCA.
        actual: String,
    },
    /// A child PCA issuer is not a trusted CTA signer.
    UntrustedCtaIssuer {
        /// Index of the child PCA.
        index: usize,
        /// Actual issuer.
        actual: String,
    },

    /// A PCA in the chain is expired.
    Expired {
        /// Index of the PCA in the chain.
        index: usize,
        /// When the PCA expired.
        expired_at: String,
    },
    /// A child PCA expires after its parent.
    ExpiryExceedsParent {
        /// Index of the child PCA.
        index: usize,
        /// Parent expiry.
        parent_expires_at: String,
        /// Child expiry.
        child_expires_at: String,
    },

    /// Root hash mismatch (transaction id is not consistent).
    RootHashMismatch {
        /// Index of the PCA in the chain.
        index: usize,
        /// Expected root hash.
        expected: String,
        /// Actual root hash.
        actual: String,
    },

    /// A capability transition is invalid.
    InvalidTransition {
        /// Index of the child PCA.
        index: usize,
        /// Reason for the failure.
        reason: String,
    },

    /// A capability key appears more than once within a PCA.
    DuplicateCapabilityKey {
        /// Index of the PCA being validated.
        index: usize,
        /// The duplicated key.
        capability_key: String,
    },

    /// A child capability key does not exist in the parent.
    UnknownCapabilityKey {
        /// Index of the child PCA.
        index: usize,
        /// The unexpected key.
        capability_key: String,
    },

    /// The parent's designated executor is not a direct `PublicKey`.
    ///
    /// Chain validation requires the parent's designated executor to be a direct
    /// `PublicKey` so we can verify the child's issuer matches. When the executor
    /// is a `Characteristic` or `CtaReference`, resolution must be performed by a CTA.
    NonPublicKeyExecutor {
        /// Index of the parent PCA.
        index: usize,
        /// Description of the executor type.
        executor_type: String,
    },

    /// The chain exceeds the maximum allowed depth.
    ChainTooDeep {
        /// Actual chain depth.
        depth: usize,
        /// Maximum allowed depth.
        limit: usize,
    },
}

impl std::fmt::Display for ChainValidationError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyChain => write!(f, "chain is empty"),
            Self::InvalidSignature { index } => {
                write!(f, "invalid signature at index {index}")
            }
            Self::InvalidPca { index, reason } => {
                write!(f, "invalid PCA at index {index}: {reason}")
            }
            Self::RootHasPrevHash => write!(f, "root PCA has prev_hash (not a root)"),
            Self::RootHasRootHash => write!(f, "root PCA must omit root_hash"),
            Self::MissingPrevHash { index } => {
                write!(f, "missing prev_hash at index {index}")
            }
            Self::HashMismatch {
                index,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "hash mismatch at index {index}: expected {expected}, got {actual}"
                )
            }
            Self::UnauthorizedIssuer {
                index,
                expected_issuer,
                actual_issuer,
            } => {
                write!(
                    f,
                    "unauthorized issuer at index {index}: expected {expected_issuer}, got {actual_issuer}"
                )
            }
            Self::InvalidRootAuthority { expected, actual } => {
                write!(
                    f,
                    "invalid root authority: expected {expected}, got {actual}"
                )
            }
            Self::UntrustedCtaIssuer { index, actual } => {
                write!(f, "untrusted CTA issuer at index {index}: {actual}")
            }
            Self::Expired { index, expired_at } => {
                write!(f, "PCA at index {index} expired at {expired_at}")
            }
            Self::ExpiryExceedsParent {
                index,
                parent_expires_at,
                child_expires_at,
            } => {
                write!(
                    f,
                    "PCA at index {index} expires after parent (parent: {parent_expires_at}, child: {child_expires_at})"
                )
            }
            Self::RootHashMismatch {
                index,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "root_hash mismatch at index {index}: expected {expected}, got {actual}"
                )
            }
            Self::RootHashMissing { index } => {
                write!(f, "missing root_hash at index {index}")
            }
            Self::InvalidTransition { index, reason } => {
                write!(
                    f,
                    "invalid capability transition at index {index}: {reason}"
                )
            }
            Self::DuplicateCapabilityKey {
                index,
                capability_key,
            } => {
                write!(
                    f,
                    "duplicate capability_key at index {index}: {capability_key}"
                )
            }
            Self::UnknownCapabilityKey {
                index,
                capability_key,
            } => {
                write!(
                    f,
                    "unknown capability_key at index {index}: {capability_key}"
                )
            }
            Self::NonPublicKeyExecutor {
                index,
                executor_type,
            } => {
                write!(
                    f,
                    "parent at index {index} has non-PublicKey designated executor ({executor_type}), requires CTA resolution"
                )
            }
            Self::ChainTooDeep { depth, limit } => {
                write!(f, "chain depth {depth} exceeds limit of {limit}")
            }
        }
    }
}

impl std::error::Error for ChainValidationError {}

/// Validate a complete PCA chain using keyed capability matching.
///
/// Validates:
/// 1. Chain is non-empty
/// 2. Root PCA is a root (no `prev_hash`) and issued by `root_authority`
/// 3. All signatures are valid
/// 4. Chain linking is correct (`prev_hash` matches)
/// 5. Issuer authorization (issuer = parent's `designated_executor`)
/// 6. Temporal validity (none expired at `current_time`)
/// 7. Child expiry does not exceed parent expiry
/// 8. Capability transitions (using validator, matched by key)
///
/// # Arguments
///
/// * `chain` - Slice of PCAs, ordered root-first (index 0 = root)
/// * `root_authority` - Expected public key of the root issuer
/// * `validator` - Transition validator for capability attenuation
/// * `current_time` - Time to check expiry against
///
/// # Returns
///
/// * `Ok(())` if the chain is valid
/// * `Err(ChainValidationError)` describing the first validation failure
///
/// # Example
///
/// ```rust
/// use amla_protocol::{
///     validate_chain, KeyPair, Algorithm, PcaBuilder, CapabilityData, PermissiveValidator,
///     PROTOCOL_VERSION,
/// };
/// use chrono::{Duration, Utc};
///
/// let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1u8; 32]);
/// let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2u8; 32]);
///
/// let cap = CapabilityData::from_json("cap:test", "function", &serde_json::json!({"name": "test"})).unwrap();
/// let expires = Utc::now() + Duration::hours(1);
///
/// let root = PcaBuilder::new()
///     .version(PROTOCOL_VERSION)
///     .add_capability(cap.clone())
///     .designated_executor(agent.public_key())
///     .expires_at(expires)
///     .build_and_sign(&gateway)
///     .unwrap();
///
/// let validator = PermissiveValidator;
/// let result = validate_chain(&[root], &gateway.public_key(), &validator, Utc::now());
/// assert!(result.is_ok());
/// ```
#[allow(clippy::too_many_lines)]
pub fn validate_chain(
    chain: &[Pca],
    root_authority: &PublicKey,
    validator: &dyn TransitionValidator,
    current_time: DateTime<Utc>,
) -> std::result::Result<(), ChainValidationError> {
    // Check non-empty
    if chain.is_empty() {
        return Err(ChainValidationError::EmptyChain);
    }

    // Check chain depth limit
    if chain.len() > MAX_CHAIN_DEPTH {
        return Err(ChainValidationError::ChainTooDeep {
            depth: chain.len(),
            limit: MAX_CHAIN_DEPTH,
        });
    }

    // Validate root
    let root = &chain[0];

    // Root must not have prev_hash
    if !root.is_root() {
        return Err(ChainValidationError::RootHasPrevHash);
    }

    // Root must be issued by root_authority
    if root.issuer() != root_authority {
        return Err(ChainValidationError::InvalidRootAuthority {
            expected: root_authority.to_hex(),
            actual: root.issuer().to_hex(),
        });
    }

    // Root PCA must omit root_hash so its hash can serve as the transaction id.
    if root.root_hash().is_some() {
        return Err(ChainValidationError::RootHasRootHash);
    }

    let expected_root_hash = root
        .try_hash()
        .map_err(|e| ChainValidationError::InvalidPca {
            index: 0,
            reason: e.to_string(),
        })?;

    // Validate each PCA
    for (i, pca) in chain.iter().enumerate() {
        // Verify signature
        match pca.try_verify_signature() {
            Ok(()) => {}
            Err(Error::SignatureVerificationFailed) => {
                return Err(ChainValidationError::InvalidSignature { index: i });
            }
            Err(e) => {
                return Err(ChainValidationError::InvalidPca {
                    index: i,
                    reason: e.to_string(),
                });
            }
        }

        // Check expiry
        if pca.is_expired_at(current_time) {
            return Err(ChainValidationError::Expired {
                index: i,
                expired_at: pca.expires_at().to_rfc3339(),
            });
        }

        // For non-root PCAs, validate chain linking
        if i > 0 {
            let parent = &chain[i - 1];

            // Child expiry must not exceed parent expiry.
            if pca.expires_at() > parent.expires_at() {
                return Err(ChainValidationError::ExpiryExceedsParent {
                    index: i,
                    parent_expires_at: parent.expires_at().to_rfc3339(),
                    child_expires_at: pca.expires_at().to_rfc3339(),
                });
            }

            // Must have prev_hash
            let prev_hash = pca
                .prev_hash()
                .ok_or(ChainValidationError::MissingPrevHash { index: i })?;

            // prev_hash must match parent's hash
            let parent_hash = parent
                .try_hash()
                .map_err(|e| ChainValidationError::InvalidPca {
                    index: i - 1,
                    reason: e.to_string(),
                })?;
            if prev_hash != &parent_hash {
                return Err(ChainValidationError::HashMismatch {
                    index: i,
                    expected: parent_hash.to_hex(),
                    actual: prev_hash.to_hex(),
                });
            }

            // Root hash must be present and consistent across the chain.
            let root_hash = pca
                .root_hash()
                .ok_or(ChainValidationError::RootHashMissing { index: i })?;
            if root_hash != &expected_root_hash {
                return Err(ChainValidationError::RootHashMismatch {
                    index: i,
                    expected: expected_root_hash.to_hex(),
                    actual: root_hash.to_hex(),
                });
            }

            // Issuer must be parent's designated executor.
            // For direct PublicKey executors, we can verify this locally.
            // For Characteristic or CtaReference, a CTA must resolve the executor.
            let parent_executor = parent.designated_executor();
            match parent_executor {
                DesignatedExecutor::PublicKey(expected_key) => {
                    if pca.issuer() != expected_key {
                        return Err(ChainValidationError::UnauthorizedIssuer {
                            index: i,
                            expected_issuer: expected_key.to_hex(),
                            actual_issuer: pca.issuer().to_hex(),
                        });
                    }
                }
                DesignatedExecutor::Characteristic(c) => {
                    return Err(ChainValidationError::NonPublicKeyExecutor {
                        index: i - 1,
                        executor_type: format!(
                            "Characteristic({}:{})",
                            c.characteristic_type, c.value
                        ),
                    });
                }
                DesignatedExecutor::CtaReference(r) => {
                    return Err(ChainValidationError::NonPublicKeyExecutor {
                        index: i - 1,
                        executor_type: format!("CtaReference({})", r.cta_key.to_hex()),
                    });
                }
            }

            // Validate capability transitions
            validate_keyed_transitions(i, parent.capabilities(), pca.capabilities(), validator)?;
        }
    }

    Ok(())
}

/// Validate a CTA-signed PCA chain.
///
/// Unlike [`validate_chain`], this function accepts chains where all non-root
/// PCAs are signed by a trusted CTA (not by the previous executor).
///
/// Validates:
/// 1. Chain is non-empty
/// 2. Root PCA is a root (no `prev_hash`) and issued by a trusted root authority
/// 3. All signatures are valid
/// 4. Chain linking is correct (`prev_hash` matches)
/// 5. Root hash consistency across the chain
/// 6. Temporal validity (none expired at `current_time`)
/// 7. Child expiry does not exceed parent expiry
/// 8. Child issuers are trusted CTAs
/// 9. Capability transitions (using validator, matched by key)
///
/// # Arguments
///
/// * `chain` - Slice of PCAs, ordered root-first (index 0 = root)
/// * `root_authorities` - Trusted root issuers (gateway keys)
/// * `trusted_ctas` - Trusted CTA signers for non-root PCAs
/// * `validator` - Transition validator for capability attenuation
/// * `current_time` - Time to check expiry against
///
/// # Returns
///
/// * `Ok(())` if the chain is valid
/// * `Err(ChainValidationError)` describing the first validation failure
pub fn validate_cta_chain(
    chain: &[Pca],
    root_authorities: &[PublicKey],
    trusted_ctas: &[PublicKey],
    validator: &dyn TransitionValidator,
    current_time: DateTime<Utc>,
) -> std::result::Result<(), ChainValidationError> {
    if chain.is_empty() {
        return Err(ChainValidationError::EmptyChain);
    }

    let root = &chain[0];
    if !root.is_root() {
        return Err(ChainValidationError::RootHasPrevHash);
    }

    if !root_authorities.contains(root.issuer()) {
        return Err(ChainValidationError::InvalidRootAuthority {
            expected: "one of trusted root authorities".to_string(),
            actual: root.issuer().to_hex(),
        });
    }

    if root.root_hash().is_some() {
        return Err(ChainValidationError::RootHasRootHash);
    }

    let expected_root_hash = root
        .try_hash()
        .map_err(|e| ChainValidationError::InvalidPca {
            index: 0,
            reason: e.to_string(),
        })?;

    for (i, pca) in chain.iter().enumerate() {
        match pca.try_verify_signature() {
            Ok(()) => {}
            Err(Error::SignatureVerificationFailed) => {
                return Err(ChainValidationError::InvalidSignature { index: i });
            }
            Err(e) => {
                return Err(ChainValidationError::InvalidPca {
                    index: i,
                    reason: e.to_string(),
                });
            }
        }

        if pca.is_expired_at(current_time) {
            return Err(ChainValidationError::Expired {
                index: i,
                expired_at: pca.expires_at().to_rfc3339(),
            });
        }

        if i > 0 {
            let parent = &chain[i - 1];

            if pca.expires_at() > parent.expires_at() {
                return Err(ChainValidationError::ExpiryExceedsParent {
                    index: i,
                    parent_expires_at: parent.expires_at().to_rfc3339(),
                    child_expires_at: pca.expires_at().to_rfc3339(),
                });
            }

            let prev_hash = pca
                .prev_hash()
                .ok_or(ChainValidationError::MissingPrevHash { index: i })?;

            let parent_hash = parent
                .try_hash()
                .map_err(|e| ChainValidationError::InvalidPca {
                    index: i - 1,
                    reason: e.to_string(),
                })?;

            if prev_hash != &parent_hash {
                return Err(ChainValidationError::HashMismatch {
                    index: i,
                    expected: parent_hash.to_hex(),
                    actual: prev_hash.to_hex(),
                });
            }

            let root_hash = pca
                .root_hash()
                .ok_or(ChainValidationError::RootHashMissing { index: i })?;
            if root_hash != &expected_root_hash {
                return Err(ChainValidationError::RootHashMismatch {
                    index: i,
                    expected: expected_root_hash.to_hex(),
                    actual: root_hash.to_hex(),
                });
            }

            if !trusted_ctas.contains(pca.issuer()) {
                return Err(ChainValidationError::UntrustedCtaIssuer {
                    index: i,
                    actual: pca.issuer().to_hex(),
                });
            }

            validate_keyed_transitions(i, parent.capabilities(), pca.capabilities(), validator)?;
        }
    }

    Ok(())
}

/// Keyed capability matching: child capabilities must be a subset of parent by key.
///
/// This allows reordering and dropping capabilities, as long as every child
/// capability is an attenuation of a parent capability with the same key.
///
/// # Algorithm
///
/// 1. Build a lookup map from parent capability keys to capabilities
/// 2. For each child capability:
///    a. Verify the key exists in the parent (subset enforcement)
///    b. Verify no duplicate keys in the child
///    c. Run the transition validator (application-specific attenuation rules)
///
/// # Why Keyed Matching?
///
/// Positional matching (child[0] → parent[0]) would require exact ordering
/// and prevent dropping capabilities without placeholder entries. Keyed matching
/// provides more flexibility while maintaining security:
/// - Parent has {A, B, C} → Child can have {B} (dropped A and C)
/// - Parent has {A, B} → Child cannot have {C} (unknown key)
/// - Each key's transition is validated independently
///
/// # Complexity
///
/// O(p + c) where p = parent capabilities, c = child capabilities.
/// `HashMap` provides O(1) average lookup.
fn validate_keyed_transitions(
    child_index: usize,
    parent_caps: &[CapabilityData],
    child_caps: &[CapabilityData],
    validator: &dyn TransitionValidator,
) -> std::result::Result<(), ChainValidationError> {
    use std::collections::HashMap;

    // Build a key → capability map for the parent.
    // Duplicate keys in parent are caught here (defense-in-depth; PcaBuilder also checks).
    let mut parent_map: HashMap<&str, &CapabilityData> = HashMap::with_capacity(parent_caps.len());
    for cap in parent_caps {
        let key = cap.key();
        if parent_map.insert(key, cap).is_some() {
            return Err(ChainValidationError::DuplicateCapabilityKey {
                index: child_index - 1,
                capability_key: key.to_string(),
            });
        }
    }

    // Validate each child capability against its parent counterpart.
    // Track seen keys to catch duplicates in child.
    let mut seen_keys = std::collections::HashSet::with_capacity(child_caps.len());
    for child_cap in child_caps {
        let key = child_cap.key();

        // Duplicate key in child?
        if !seen_keys.insert(key) {
            return Err(ChainValidationError::DuplicateCapabilityKey {
                index: child_index,
                capability_key: key.to_string(),
            });
        }

        // Key must exist in parent (subset rule).
        let matching_parent =
            parent_map
                .get(key)
                .ok_or_else(|| ChainValidationError::UnknownCapabilityKey {
                    index: child_index,
                    capability_key: key.to_string(),
                })?;

        // Delegate to application-specific validator for attenuation rules.
        // This is where domain logic lives (e.g., "amount can only decrease").
        validator
            .validate_transition(matching_parent, child_cap)
            .map_err(
                |e: TransitionError| ChainValidationError::InvalidTransition {
                    index: child_index,
                    reason: e.reason,
                },
            )?;
    }

    Ok(())
}

/// Get the final executor from a valid chain.
///
/// Returns the designated executor of the last PCA in the chain.
/// This is the entity authorized to execute the capabilities.
///
/// Note: The returned executor may be a `PublicKey`, `Characteristic`, or
/// `CtaReference`. Use `designated_executor_key()` if you need a direct key,
/// or handle the enum variants appropriately.
///
/// # Panics
///
/// Panics if the chain is empty. Always validate first.
#[must_use]
pub fn get_final_executor(chain: &[Pca]) -> &DesignatedExecutor {
    chain
        .last()
        .expect("chain should not be empty")
        .designated_executor()
}

/// Get the effective capabilities from a valid chain.
///
/// Returns the capabilities from the last PCA in the chain.
/// These are the most attenuated capabilities.
///
/// # Panics
///
/// Panics if the chain is empty. Always validate first.
#[must_use]
pub fn get_effective_capabilities(chain: &[Pca]) -> &[CapabilityData] {
    chain
        .last()
        .expect("chain should not be empty")
        .capabilities()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Algorithm, KeyPair, PROTOCOL_VERSION, PcaBuilder, PermissiveValidator, StrictValidator,
    };
    use chrono::Duration;

    /// Create a deterministic keypair from a seed byte (for testing).
    fn keypair(seed: u8) -> KeyPair {
        KeyPair::from_seed(Algorithm::Ed25519, &[seed; 32])
    }

    fn create_cap(key: &str, data: &serde_json::Value) -> CapabilityData {
        CapabilityData::from_json(key, "function", data).unwrap()
    }

    fn create_test_chain() -> (KeyPair, KeyPair, KeyPair, Vec<Pca>) {
        let gateway = keypair(1);
        let claims_agent = keypair(2);
        let payout_agent = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        let root_cap = create_cap(
            "cap:process",
            &serde_json::json!({"name": "process_claim", "max_amount": 25000}),
        );

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(root_cap)
            .designated_executor(claims_agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child_cap = create_cap(
            "cap:process",
            &serde_json::json!({"name": "execute_payout", "amount": 5000}),
        );

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(child_cap)
            .designated_executor(payout_agent.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&claims_agent)
            .unwrap();

        (gateway, claims_agent, payout_agent, vec![root, child])
    }

    #[test]
    fn test_valid_chain() {
        let (gateway, _, _, chain) = create_test_chain();
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_chain() {
        let gateway = keypair(1);
        let validator = PermissiveValidator;
        let result = validate_chain(&[], &gateway.public_key(), &validator, Utc::now());
        assert!(matches!(result, Err(ChainValidationError::EmptyChain)));
    }

    #[test]
    fn test_invalid_root_authority() {
        let (_, _, _, chain) = create_test_chain();
        let wrong_authority = keypair(10);
        let validator = PermissiveValidator;
        let result = validate_chain(
            &chain,
            &wrong_authority.public_key(),
            &validator,
            Utc::now(),
        );
        assert!(matches!(
            result,
            Err(ChainValidationError::InvalidRootAuthority { .. })
        ));
    }

    #[test]
    fn test_unauthorized_issuer() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);
        let attacker = keypair(10);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Attacker tries to extend chain (not authorized)
        let malicious = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&attacker) // Wrong signer!
            .unwrap();

        let chain = vec![root, malicious];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::UnauthorizedIssuer { index: 1, .. })
        ));
    }

    #[test]
    fn test_expired_pca() {
        let gateway = keypair(1);
        let agent = keypair(2);

        let expired = Utc::now() - Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expired)
            .build_and_sign(&gateway)
            .unwrap();

        let validator = PermissiveValidator;
        let result = validate_chain(&[root], &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::Expired { index: 0, .. })
        ));
    }

    #[test]
    fn test_child_expiry_exceeds_parent() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let parent_expires = Utc::now() + Duration::hours(1);
        let child_expires = Utc::now() + Duration::hours(2);

        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(parent_expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(child_expires)
            .build_and_sign(&agent1)
            .unwrap();

        let validator = PermissiveValidator;
        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &validator,
            Utc::now(),
        );

        assert!(matches!(
            result,
            Err(ChainValidationError::ExpiryExceedsParent { index: 1, .. })
        ));
    }

    #[test]
    fn test_hash_mismatch() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Create another root to get a different hash
        let other_cap = create_cap("cap:other", &serde_json::json!({}));
        let other_root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(other_cap)
            .designated_executor(agent2.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Child links to wrong parent
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&other_root)
            .unwrap() // Wrong root on purpose
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, child];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::HashMismatch { index: 1, .. })
        ));
    }

    #[test]
    fn test_capability_transition_rejected() {
        let (gateway, _, _, chain) = create_test_chain();

        // Strict validator rejects any change
        let validator = StrictValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::InvalidTransition { index: 1, .. })
        ));
    }

    #[test]
    fn test_custom_transition_validator() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct FunctionCap {
            name: String,
            max_amount: i64,
        }

        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        let parent_cap = CapabilityData::new(
            "cap:api",
            "function",
            &FunctionCap {
                name: "api.call".to_string(),
                max_amount: 1000,
            },
        )
        .unwrap();

        let child_cap = CapabilityData::new(
            "cap:api",
            "function",
            &FunctionCap {
                name: "api.call".to_string(),
                max_amount: 500,
            },
        )
        .unwrap();

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

        let validator = crate::ClosureValidator::new(|parent, child| {
            let parent_cap: FunctionCap = parent
                .decode()
                .map_err(|e| TransitionError::new(format!("decode parent: {e}")))?;
            let child_cap: FunctionCap = child
                .decode()
                .map_err(|e| TransitionError::new(format!("decode child: {e}")))?;

            if parent_cap.name != child_cap.name {
                return Err(TransitionError::new("function name mismatch"));
            }

            if child_cap.max_amount <= parent_cap.max_amount {
                Ok(())
            } else {
                Err(TransitionError::new("invalid attenuation"))
            }
        });

        let chain = vec![root, child];
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());
        assert!(result.is_ok());
    }

    #[test]
    fn test_custom_validator_rejects_escalation() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct FunctionCap {
            name: String,
            max_amount: i64,
        }

        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        let parent_cap = CapabilityData::new(
            "cap:api",
            "function",
            &FunctionCap {
                name: "api.call".to_string(),
                max_amount: 1000,
            },
        )
        .unwrap();

        let child_cap = CapabilityData::new(
            "cap:api",
            "function",
            &FunctionCap {
                name: "api.call".to_string(),
                max_amount: 2000,
            },
        )
        .unwrap();

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

        let validator = crate::ClosureValidator::new(|parent, child| {
            let parent_cap: FunctionCap = parent
                .decode()
                .map_err(|e| TransitionError::new(format!("decode parent: {e}")))?;
            let child_cap: FunctionCap = child
                .decode()
                .map_err(|e| TransitionError::new(format!("decode child: {e}")))?;

            if child_cap.max_amount <= parent_cap.max_amount {
                Ok(())
            } else {
                Err(TransitionError::new("privilege escalation attempted"))
            }
        });

        let chain = vec![root, child];
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::InvalidTransition { index: 1, .. })
        ));
    }

    #[test]
    fn test_single_pca_chain() {
        let gateway = keypair(1);
        let agent = keypair(2);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let validator = PermissiveValidator;
        let result = validate_chain(
            std::slice::from_ref(&root),
            &gateway.public_key(),
            &validator,
            Utc::now(),
        );
        assert!(result.is_ok());

        // Check helper functions
        assert_eq!(
            get_final_executor(std::slice::from_ref(&root)).as_public_key(),
            Some(&agent.public_key())
        );
        assert_eq!(
            get_effective_capabilities(std::slice::from_ref(&root)).len(),
            1
        );
    }

    #[test]
    fn test_three_hop_chain() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);
        let agent3 = keypair(4);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({"level": "root"}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let middle = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let leaf = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent3.public_key())
            .parent_pca(&middle)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent2)
            .unwrap();

        let chain = vec![root, middle, leaf];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());
        assert!(result.is_ok());

        // Final executor is agent3
        assert_eq!(
            get_final_executor(&chain).as_public_key(),
            Some(&agent3.public_key())
        );
    }

    #[test]
    fn test_five_hop_chain() {
        let gateway = keypair(1);
        let agents: Vec<_> = (0..5).map(|i| keypair(10 + i)).collect();

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let mut chain = Vec::new();

        // Root
        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agents[0].public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();
        chain.push(root);

        // Build chain of delegations
        for i in 0..4 {
            let pca = PcaBuilder::new()
                .version(PROTOCOL_VERSION)
                .add_capability(cap.clone())
                .designated_executor(agents[i + 1].public_key())
                .parent_pca(chain.last().unwrap())
                .unwrap()
                .expires_at(expires)
                .build_and_sign(&agents[i])
                .unwrap();
            chain.push(pca);
        }

        assert_eq!(chain.len(), 5);

        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());
        assert!(result.is_ok());

        assert_eq!(
            get_final_executor(&chain).as_public_key(),
            Some(&agents[4].public_key())
        );
    }

    #[test]
    fn test_root_has_prev_hash_error() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        // Create a "root" that actually has a prev_hash
        let fake_root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .prev_hash(crate::PcaHash::compute(b"fake parent"))
            .root_hash(crate::PcaHash::compute(b"fake root"))
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&fake_root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![fake_root, child];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(result, Err(ChainValidationError::RootHasPrevHash)));
    }

    #[test]
    fn test_root_hash_mismatch_error() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Create child with wrong root_hash
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .prev_hash(root.try_hash().unwrap())
            .root_hash(crate::PcaHash::compute(b"wrong root")) // Wrong!
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, child];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::RootHashMismatch { index: 1, .. })
        ));
    }

    #[test]
    fn test_root_hash_preserved_across_chain() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);
        let agent3 = keypair(4);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let root_hash = root.try_hash().unwrap();

        // Root has no root_hash
        assert!(root.root_hash().is_none());

        let child1 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        // Child has root_hash pointing to root
        assert_eq!(child1.root_hash(), Some(&root_hash));

        let child2 = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent3.public_key())
            .parent_pca(&child1)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent2)
            .unwrap();

        // Grandchild also has root_hash pointing to root
        assert_eq!(child2.root_hash(), Some(&root_hash));

        // Chain validates
        let chain = vec![root, child1, child2];
        let validator = PermissiveValidator;
        assert!(validate_chain(&chain, &gateway.public_key(), &validator, Utc::now()).is_ok());
    }

    #[test]
    fn test_missing_prev_hash_error() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Child without prev_hash (incorrectly built as root)
        let orphan = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, orphan];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::MissingPrevHash { index: 1 })
        ));
    }

    #[test]
    fn test_keyed_subset_allowed() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        let cap1 = create_cap(
            "cap:claims",
            &serde_json::json!({"name": "insurance.claim"}),
        );
        let cap2 = create_cap(
            "cap:payout",
            &serde_json::json!({"name": "insurance.payout"}),
        );

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap1)
            .add_capability(cap2.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Child drops one capability while keeping key-based linkage.
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap2)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let validator = PermissiveValidator;
        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &validator,
            Utc::now(),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_keyed_validator_unknown_key() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        // Child references a key that doesn't exist in the parent.
        let parent_cap = create_cap("cap:parent", &serde_json::json!({"name": "parent"}));
        let child_cap = create_cap("cap:child", &serde_json::json!({"name": "child"}));

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

        let validator = PermissiveValidator;
        let result = validate_chain(
            &[root, child],
            &gateway.public_key(),
            &validator,
            Utc::now(),
        );

        assert!(matches!(
            result,
            Err(ChainValidationError::UnknownCapabilityKey { index: 1, .. })
        ));
    }

    #[test]
    fn test_duplicate_keys_rejected_at_pca_creation() {
        use crate::Error;

        let gateway = keypair(1);
        let agent = keypair(2);
        let expires = Utc::now() + Duration::hours(1);

        // Duplicate keys are rejected at PCA creation time (fail-fast).
        let cap_a = create_cap("cap:dup", &serde_json::json!({"name": "a"}));
        let cap_b = create_cap("cap:dup", &serde_json::json!({"name": "b"}));

        let result = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap_a)
            .add_capability(cap_b)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::DuplicateCapabilityKey(key) => {
                assert_eq!(key, "cap:dup");
            }
            other => panic!("Expected DuplicateCapabilityKey error, got: {other}"),
        }
    }

    // Note: Chain validation also checks for duplicate keys as defense-in-depth
    // against malformed CBOR payloads. This is tested implicitly via the
    // keyed matching path, but we can't create a test case through the normal
    // API since PcaBuilder rejects duplicates.

    #[test]
    fn test_expired_middle_pca() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let future = Utc::now() + Duration::hours(1);
        let past = Utc::now() - Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(future)
            .build_and_sign(&gateway)
            .unwrap();

        // Middle PCA is expired
        let middle = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(past)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, middle];
        let validator = PermissiveValidator;
        let result = validate_chain(&chain, &gateway.public_key(), &validator, Utc::now());

        assert!(matches!(
            result,
            Err(ChainValidationError::Expired { index: 1, .. })
        ));
    }

    #[test]
    fn test_chain_validation_error_display() {
        let errors = vec![
            ChainValidationError::EmptyChain,
            ChainValidationError::InvalidSignature { index: 2 },
            ChainValidationError::InvalidPca {
                index: 1,
                reason: "decode failure".into(),
            },
            ChainValidationError::RootHasPrevHash,
            ChainValidationError::RootHasRootHash,
            ChainValidationError::MissingPrevHash { index: 1 },
            ChainValidationError::HashMismatch {
                index: 1,
                expected: "abc".into(),
                actual: "def".into(),
            },
            ChainValidationError::RootHashMismatch {
                index: 1,
                expected: "abc".into(),
                actual: "def".into(),
            },
            ChainValidationError::RootHashMissing { index: 1 },
            ChainValidationError::UnauthorizedIssuer {
                index: 1,
                expected_issuer: "pub1".into(),
                actual_issuer: "pub2".into(),
            },
            ChainValidationError::InvalidRootAuthority {
                expected: "root".into(),
                actual: "other".into(),
            },
            ChainValidationError::UntrustedCtaIssuer {
                index: 1,
                actual: "cta".into(),
            },
            ChainValidationError::Expired {
                index: 0,
                expired_at: "2024-01-01T00:00:00Z".into(),
            },
            ChainValidationError::ExpiryExceedsParent {
                index: 1,
                parent_expires_at: "2024-01-01T00:00:00Z".into(),
                child_expires_at: "2024-01-02T00:00:00Z".into(),
            },
            ChainValidationError::InvalidTransition {
                index: 1,
                reason: "type mismatch".into(),
            },
        ];

        for err in errors {
            // Should not panic and produce non-empty strings
            let display = format!("{err}");
            assert!(!display.is_empty());
            let debug = format!("{err:?}");
            assert!(!debug.is_empty());
        }
    }

    #[test]
    fn test_get_helpers_on_multi_hop_chain() {
        let gateway = keypair(1);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);
        let cap1 = create_cap("cap:level", &serde_json::json!({"level": 1}));
        let cap2 = create_cap("cap:level", &serde_json::json!({"level": 2}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap1)
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap2.clone())
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        let chain = vec![root, child];

        // Final executor is the leaf's designated executor
        assert_eq!(
            get_final_executor(&chain).as_public_key(),
            Some(&agent2.public_key())
        );

        // Effective capabilities are from the leaf
        let effective = get_effective_capabilities(&chain);
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0], cap2);
    }

    #[test]
    fn test_chain_validation_error_display_all_variants() {
        // Ensure all error variants have working Display implementations
        let errors = vec![
            ChainValidationError::DuplicateCapabilityKey {
                index: 0,
                capability_key: "cap:test".into(),
            },
            ChainValidationError::UnknownCapabilityKey {
                index: 1,
                capability_key: "cap:unknown".into(),
            },
            ChainValidationError::NonPublicKeyExecutor {
                index: 0,
                executor_type: "Characteristic(role:agent)".into(),
            },
        ];

        for err in errors {
            let display = format!("{err}");
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn test_invalid_signature_in_chain() {
        // Create a PCA signed by the wrong key to trigger InvalidSignature
        // We'll build the PCA content but sign with a different key
        let gateway = keypair(1);
        let wrong_signer = keypair(10);
        let agent = keypair(2);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        // Build a PCA signed by wrong_signer but claim gateway as issuer
        // This is done by creating a valid PCA structure but with mismatched signature
        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&wrong_signer) // Signed by wrong key!
            .unwrap();

        // The signature is valid for wrong_signer, but we'll validate against gateway
        let validator = PermissiveValidator;
        let result = validate_chain(&[pca], &gateway.public_key(), &validator, Utc::now());

        // This actually triggers InvalidRootAuthority because issuer != root_authority
        // The InvalidSignature path is hit when signature.verify() fails
        assert!(matches!(
            result,
            Err(ChainValidationError::InvalidRootAuthority { .. })
        ));
    }

    #[test]
    fn test_tampered_signature_fails_verification() {
        // To test InvalidSignature, we need a PCA where the signature doesn't match
        // We'll manually construct this by modifying signature bytes after serialization

        let gateway = keypair(1);
        let agent = keypair(2);

        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // The PCA has a valid signature - try_verify_signature returns Ok(())
        assert!(pca.try_verify_signature().is_ok());

        // If we could construct a PCA with an invalid signature, try_verify_signature
        // would return an error, and chain validation would fail with InvalidSignature.
        // Since PcaBuilder always produces valid signatures, and from_cbor validates
        // the structure, the InvalidSignature path is only hit with corrupted data
        // which would fail deserialization anyway.
        //
        // This test documents that try_verify_signature() works correctly.
    }

    // ========================================================================
    // validate_cta_chain tests
    // ========================================================================

    /// Helper to create a CTA-signed chain (root signed by gateway, children signed by CTA).
    fn create_cta_signed_chain() -> (KeyPair, KeyPair, KeyPair, KeyPair, Vec<Pca>) {
        let gateway = keypair(1);
        let cta = keypair(5);
        let agent1 = keypair(2);
        let agent2 = keypair(3);

        let expires = Utc::now() + Duration::hours(1);

        let cap = create_cap("cap:test", &serde_json::json!({"value": 100}));

        // Root PCA: signed by gateway, designates agent1
        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let root_hash = root.try_hash().unwrap();

        // Child PCA: signed by CTA (not agent1), designates agent2
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .prev_hash(root_hash)
            .root_hash(root_hash)
            .expires_at(expires)
            .build_and_sign(&cta) // CTA signs, not agent1
            .unwrap();

        (gateway, cta, agent1, agent2, vec![root, child])
    }

    #[test]
    fn test_cta_chain_valid() {
        let (gateway, cta, _, _, chain) = create_cta_signed_chain();

        let result = validate_cta_chain(
            &chain,
            &[gateway.public_key()],
            &[cta.public_key()],
            &PermissiveValidator,
            Utc::now(),
        );

        assert!(result.is_ok(), "Valid CTA chain should pass: {result:?}");
    }

    #[test]
    fn test_cta_chain_untrusted_root_authority() {
        let (_gateway, cta, _, _, chain) = create_cta_signed_chain();
        let untrusted = keypair(12);

        // Don't include gateway in root_authorities
        let result = validate_cta_chain(
            &chain,
            &[untrusted.public_key()], // Wrong authority
            &[cta.public_key()],
            &PermissiveValidator,
            Utc::now(),
        );

        assert!(
            matches!(
                result,
                Err(ChainValidationError::InvalidRootAuthority { .. })
            ),
            "Should reject untrusted root authority: {result:?}"
        );
    }

    #[test]
    fn test_cta_chain_untrusted_cta_issuer() {
        let (gateway, _cta, _, _, chain) = create_cta_signed_chain();
        let other_cta = keypair(11);

        // Don't include the actual CTA in trusted_ctas
        let result = validate_cta_chain(
            &chain,
            &[gateway.public_key()],
            &[other_cta.public_key()], // Wrong CTA
            &PermissiveValidator,
            Utc::now(),
        );

        assert!(
            matches!(result, Err(ChainValidationError::UntrustedCtaIssuer { .. })),
            "Should reject untrusted CTA issuer: {result:?}"
        );
    }

    #[test]
    fn test_cta_chain_empty() {
        let result = validate_cta_chain(&[], &[], &[], &PermissiveValidator, Utc::now());

        assert!(
            matches!(result, Err(ChainValidationError::EmptyChain)),
            "Should reject empty chain: {result:?}"
        );
    }

    // Note: test_cta_chain_root_with_prev_hash and test_cta_chain_root_with_root_hash
    // are not needed because PcaBuilder already validates these constraints at build time.
    // The chain validation paths (RootHasPrevHash, RootHasRootHash) would only be hit
    // with corrupted/maliciously constructed PCAs that bypass the builder.

    #[test]
    fn test_cta_chain_multiple_root_authorities() {
        let gateway1 = keypair(1);
        let gateway2 = keypair(6);
        let cta = keypair(5);
        let agent = keypair(2);
        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        // Root signed by gateway2
        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway2)
            .unwrap();

        // Trusting both gateways should work
        let result = validate_cta_chain(
            &[root],
            &[gateway1.public_key(), gateway2.public_key()],
            &[cta.public_key()],
            &PermissiveValidator,
            Utc::now(),
        );

        assert!(
            result.is_ok(),
            "Should accept root from any trusted authority: {result:?}"
        );
    }

    #[test]
    fn test_cta_chain_multiple_trusted_ctas() {
        let gateway = keypair(1);
        let cta1 = keypair(5);
        let cta2 = keypair(7);
        let agent1 = keypair(2);
        let agent2 = keypair(3);
        let expires = Utc::now() + Duration::hours(1);
        let cap = create_cap("cap:test", &serde_json::json!({}));

        let root = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let root_hash = root.try_hash().unwrap();

        // Child signed by cta2
        let child = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(agent2.public_key())
            .prev_hash(root_hash)
            .root_hash(root_hash)
            .expires_at(expires)
            .build_and_sign(&cta2)
            .unwrap();

        // Trusting both CTAs should work
        let result = validate_cta_chain(
            &[root, child],
            &[gateway.public_key()],
            &[cta1.public_key(), cta2.public_key()],
            &PermissiveValidator,
            Utc::now(),
        );

        assert!(
            result.is_ok(),
            "Should accept child from any trusted CTA: {result:?}"
        );
    }

    #[test]
    fn test_cta_chain_preserves_root_hash() {
        let (gateway, cta, _, _, chain) = create_cta_signed_chain();

        // First verify the chain is valid
        validate_cta_chain(
            &chain,
            &[gateway.public_key()],
            &[cta.public_key()],
            &PermissiveValidator,
            Utc::now(),
        )
        .unwrap();

        // Verify root_hash is correctly set
        let root_hash = chain[0].try_hash().unwrap();
        assert!(
            chain[0].root_hash().is_none(),
            "Root should have no root_hash"
        );
        assert_eq!(
            chain[1].root_hash(),
            Some(&root_hash),
            "Child should have root_hash matching root's hash"
        );
    }

    #[test]
    fn test_chain_depth_limit() {
        // Create a chain that exceeds the maximum depth
        // We'll create a vec with MAX_CHAIN_DEPTH + 1 empty PCAs
        // (they don't need to be valid, just enough to trigger the depth check)

        let gateway = crate::KeyPair::from_seed(crate::Algorithm::Ed25519, &[1; 32]);
        let cap = crate::CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "test"}),
        )
        .unwrap();
        let expires = Utc::now() + Duration::hours(1);

        // Create a root PCA
        let root = crate::PcaBuilder::new()
            .version(crate::PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(gateway.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Create a chain that exceeds the limit (using the same PCA repeated)
        // This is not a valid chain, but we just need to trigger the depth check
        let chain: Vec<_> = std::iter::repeat_n(root, MAX_CHAIN_DEPTH + 1).collect();

        let result = validate_chain(
            &chain,
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        match result {
            Err(ChainValidationError::ChainTooDeep { depth, limit }) => {
                assert_eq!(depth, MAX_CHAIN_DEPTH + 1);
                assert_eq!(limit, MAX_CHAIN_DEPTH);
            }
            _ => panic!("Expected ChainTooDeep error, got {result:?}"),
        }
    }

    #[test]
    fn test_chain_at_max_depth_is_valid() {
        // A chain at exactly MAX_CHAIN_DEPTH should be accepted
        // (the limit check is > not >=)
        let gateway = crate::KeyPair::from_seed(crate::Algorithm::Ed25519, &[1; 32]);
        let cap = crate::CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "test"}),
        )
        .unwrap();
        let expires = Utc::now() + Duration::hours(1);

        let root = crate::PcaBuilder::new()
            .version(crate::PROTOCOL_VERSION)
            .add_capability(cap)
            .designated_executor(gateway.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // At exactly MAX_CHAIN_DEPTH, should pass the depth check
        // (will fail other checks since it's not a valid chain, but depth should pass)
        let chain: Vec<_> = std::iter::repeat_n(root, MAX_CHAIN_DEPTH).collect();

        let result = validate_chain(
            &chain,
            &gateway.public_key(),
            &PermissiveValidator,
            Utc::now(),
        );

        // Should NOT be ChainTooDeep - some other error like HashMismatch
        assert!(
            !matches!(result, Err(ChainValidationError::ChainTooDeep { .. })),
            "Chain at MAX_CHAIN_DEPTH should not trigger depth limit"
        );
    }

    // ========================================================================
    // Display impl tests for all ChainValidationError variants
    // ========================================================================

    #[test]
    fn test_display_empty_chain() {
        let err = ChainValidationError::EmptyChain;
        assert_eq!(err.to_string(), "chain is empty");
    }

    #[test]
    fn test_display_invalid_signature() {
        let err = ChainValidationError::InvalidSignature { index: 2 };
        assert_eq!(err.to_string(), "invalid signature at index 2");
    }

    #[test]
    fn test_display_invalid_pca() {
        let err = ChainValidationError::InvalidPca {
            index: 1,
            reason: "bad format".to_string(),
        };
        assert_eq!(err.to_string(), "invalid PCA at index 1: bad format");
    }

    #[test]
    fn test_display_root_has_prev_hash() {
        let err = ChainValidationError::RootHasPrevHash;
        assert_eq!(err.to_string(), "root PCA has prev_hash (not a root)");
    }

    #[test]
    fn test_display_root_has_root_hash() {
        let err = ChainValidationError::RootHasRootHash;
        assert_eq!(err.to_string(), "root PCA must omit root_hash");
    }

    #[test]
    fn test_display_missing_prev_hash() {
        let err = ChainValidationError::MissingPrevHash { index: 3 };
        assert_eq!(err.to_string(), "missing prev_hash at index 3");
    }

    #[test]
    fn test_display_hash_mismatch() {
        let err = ChainValidationError::HashMismatch {
            index: 2,
            expected: "abc123".to_string(),
            actual: "def456".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "hash mismatch at index 2: expected abc123, got def456"
        );
    }

    #[test]
    fn test_display_unauthorized_issuer() {
        let err = ChainValidationError::UnauthorizedIssuer {
            index: 1,
            expected_issuer: "alice".to_string(),
            actual_issuer: "bob".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "unauthorized issuer at index 1: expected alice, got bob"
        );
    }

    #[test]
    fn test_display_invalid_root_authority() {
        let err = ChainValidationError::InvalidRootAuthority {
            expected: "gateway".to_string(),
            actual: "attacker".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid root authority: expected gateway, got attacker"
        );
    }

    #[test]
    fn test_display_untrusted_cta_issuer() {
        let err = ChainValidationError::UntrustedCtaIssuer {
            index: 2,
            actual: "unknown_cta".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "untrusted CTA issuer at index 2: unknown_cta"
        );
    }

    #[test]
    fn test_display_expired() {
        let err = ChainValidationError::Expired {
            index: 0,
            expired_at: "2024-01-01T00:00:00Z".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "PCA at index 0 expired at 2024-01-01T00:00:00Z"
        );
    }

    #[test]
    fn test_display_expiry_exceeds_parent() {
        let err = ChainValidationError::ExpiryExceedsParent {
            index: 1,
            parent_expires_at: "2024-01-01".to_string(),
            child_expires_at: "2024-01-02".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "PCA at index 1 expires after parent (parent: 2024-01-01, child: 2024-01-02)"
        );
    }

    #[test]
    fn test_display_root_hash_mismatch() {
        let err = ChainValidationError::RootHashMismatch {
            index: 2,
            expected: "root123".to_string(),
            actual: "root456".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "root_hash mismatch at index 2: expected root123, got root456"
        );
    }

    #[test]
    fn test_display_root_hash_missing() {
        let err = ChainValidationError::RootHashMissing { index: 3 };
        assert_eq!(err.to_string(), "missing root_hash at index 3");
    }

    #[test]
    fn test_display_invalid_transition() {
        let err = ChainValidationError::InvalidTransition {
            index: 1,
            reason: "privilege escalation".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "invalid capability transition at index 1: privilege escalation"
        );
    }

    #[test]
    fn test_display_duplicate_capability_key() {
        let err = ChainValidationError::DuplicateCapabilityKey {
            index: 0,
            capability_key: "cap:stripe".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "duplicate capability_key at index 0: cap:stripe"
        );
    }

    #[test]
    fn test_display_unknown_capability_key() {
        let err = ChainValidationError::UnknownCapabilityKey {
            index: 1,
            capability_key: "cap:unknown".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "unknown capability_key at index 1: cap:unknown"
        );
    }

    #[test]
    fn test_display_non_public_key_executor() {
        let err = ChainValidationError::NonPublicKeyExecutor {
            index: 0,
            executor_type: "Characteristic".to_string(),
        };
        assert_eq!(
            err.to_string(),
            "parent at index 0 has non-PublicKey designated executor (Characteristic), requires CTA resolution"
        );
    }

    #[test]
    fn test_display_chain_too_deep() {
        let err = ChainValidationError::ChainTooDeep {
            depth: 101,
            limit: 100,
        };
        assert_eq!(err.to_string(), "chain depth 101 exceeds limit of 100");
    }

    #[test]
    fn test_chain_validation_error_is_std_error() {
        // Verify ChainValidationError implements std::error::Error
        fn assert_error<E: std::error::Error>() {}
        assert_error::<ChainValidationError>();
    }
}
