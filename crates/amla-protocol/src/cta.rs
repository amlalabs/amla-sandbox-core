//! Causal Transaction Authority (CTA) - validates continuity and emits PCAs.
//!
//! The CTA is the core validation component of the PIC model. It:
//! - Validates Proof of Continuity (`PoC`)
//! - Verifies executor matches designation
//! - Emits the next PCA if valid
//!
//! The CTA does NOT:
//! - Store or reconstruct chains (chain is virtual)
//! - Mint identity or assign authority
//! - Extend or escalate capability
//!
//! # Example
//!
//! ```rust
//! use amla_protocol::{
//!     Algorithm, CapabilityData, Cta, CtaBuilder, ContinuationRequest, DesignatedExecutor,
//!     FreshnessChallenge, KeyPair, PcaBuilder, PermissiveFreshnessValidator, PermissiveValidator,
//!     ProofOfContinuity, RejectAllResolver, PROTOCOL_VERSION,
//! };
//! use chrono::{Duration, Utc};
//!
//! // Setup: Gateway, CTA, and Agents with deterministic seeds
//! let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1u8; 32]);
//! let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &[2u8; 32]);
//! let claims_agent = KeyPair::from_seed(Algorithm::Ed25519, &[3u8; 32]);
//! let payout_agent = KeyPair::from_seed(Algorithm::Ed25519, &[4u8; 32]);
//!
//! // Create CTA that trusts the gateway
//! let cta = CtaBuilder::new(cta_keypair.clone())
//!     .trust_root_authority(gateway.public_key())
//!     .build();
//!
//! // Gateway creates root PCA for Claims Agent
//! let cap = CapabilityData::from_json(
//!     "cap:claims",
//!     "function",
//!     &serde_json::json!({"name": "process_claim", "max_amount": 25000}),
//! ).unwrap();
//!
//! let expires = Utc::now() + Duration::hours(1);
//! let root_pca = PcaBuilder::new()
//!     .version(PROTOCOL_VERSION)
//!     .add_capability(cap.clone())
//!     .designated_executor(claims_agent.public_key())
//!     .expires_at(expires)
//!     .build_and_sign(&gateway)
//!     .unwrap();
//!
//! // Claims Agent builds continuation request for Payout Agent
//! let continuation = ContinuationRequest {
//!     capabilities: vec![cap],
//!     designated_executor: DesignatedExecutor::from_public_key(payout_agent.public_key()),
//!     expires_at: expires,
//!     payload: None,
//! };
//!
//! // Claims Agent creates Proof of Continuity
//! // In production, the nonce would come from the host's CSPRNG
//! let challenge = FreshnessChallenge::from_bytes([0x42u8; 32]);
//! let poc = ProofOfContinuity::build(
//!     &root_pca,
//!     &claims_agent,
//!     &continuation,
//!     challenge,
//! ).unwrap();
//!
//! // Submit to CTA
//! let child_pca = cta.submit(&root_pca, &poc, &continuation, Utc::now()).unwrap();
//!
//! // Child PCA is now valid and designates Payout Agent
//! assert!(child_pca.try_verify_signature().is_ok());
//! ```

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use crate::{
    CapabilityData, CtaReference, DesignatedExecutor, Error, ExecutorCharacteristic, KeyPair, Pca,
    PcaBuilder, PcaHash, PublicKey, Signature, TransitionError, TransitionValidator,
    canonical_cbor_encode,
};

// ============================================================================
// Freshness Challenge
// ============================================================================

/// Domain separator for `PoC` signatures to prevent cross-protocol replay.
pub const POC_DOMAIN_SEPARATOR: &[u8] = b"PIC:PoC:v1:";

/// Freshness challenge mechanism (PIC Causal Challenge).
///
/// Extensible to support different freshness schemes based on deployment needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
#[non_exhaustive]
pub enum FreshnessChallenge {
    /// Random 32-byte nonce. Simple, stateless, widely applicable.
    #[serde(rename = "random")]
    Random(#[serde(with = "serde_bytes")] [u8; 32]),

    /// Timestamp with max skew. Useful when clock sync is available.
    #[serde(rename = "timestamp")]
    Timestamp {
        /// Unix timestamp in milliseconds.
        unix_millis: u64,
        /// Maximum allowed clock skew in milliseconds.
        max_skew_ms: u32,
    },
}

impl FreshnessChallenge {
    /// Create a random challenge from externally-provided bytes.
    ///
    /// Use this in WASM environments where random bytes are provided by the host.
    /// The bytes MUST be cryptographically random (32 bytes of entropy).
    ///
    /// # Example
    ///
    /// ```
    /// use amla_protocol::FreshnessChallenge;
    ///
    /// // Host provides random bytes
    /// let nonce: [u8; 32] = [0x42; 32]; // In practice, from host's CSPRNG
    /// let challenge = FreshnessChallenge::from_bytes(nonce);
    /// ```
    #[must_use]
    pub const fn from_bytes(nonce: [u8; 32]) -> Self {
        Self::Random(nonce)
    }

    /// Create a timestamp challenge with current time.
    ///
    /// This method uses wall-clock time and is NOT available in WASM builds.
    /// For WASM, use [`Self::timestamp_at`] with a host-provided timestamp.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // timestamp_millis is always positive after 1970
    #[allow(clippy::cast_possible_truncation)] // clamped to u32::MAX before cast
    pub fn timestamp(max_skew: Duration) -> Self {
        let unix_millis = Utc::now().timestamp_millis() as u64;
        Self::timestamp_at_millis(unix_millis, max_skew)
    }

    /// Create a timestamp challenge with a specific time.
    ///
    /// Use this in WASM environments where time is provided by the host.
    #[must_use]
    #[allow(clippy::cast_sign_loss)]
    #[allow(clippy::cast_possible_truncation)]
    pub fn timestamp_at(time: DateTime<Utc>, max_skew: Duration) -> Self {
        let unix_millis = time.timestamp_millis() as u64;
        Self::timestamp_at_millis(unix_millis, max_skew)
    }

    /// Create a timestamp challenge from milliseconds since Unix epoch.
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn timestamp_at_millis(unix_millis: u64, max_skew: Duration) -> Self {
        let max_skew_ms = max_skew
            .num_milliseconds()
            .unsigned_abs()
            .min(u64::from(u32::MAX)) as u32;
        Self::Timestamp {
            unix_millis,
            max_skew_ms,
        }
    }

    /// Encode to bytes for inclusion in `PoC` message.
    ///
    /// # Panics
    ///
    /// Panics if CBOR encoding fails (should never happen for valid types).
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        canonical_cbor_encode(self).expect("FreshnessChallenge must be serializable")
    }
}

// ============================================================================
// Freshness Validation
// ============================================================================

/// Error from freshness validation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FreshnessError {
    /// Timestamp skew exceeds maximum allowed.
    TimestampSkew,

    /// Freshness challenge already seen (replay).
    Replay,
}

impl std::fmt::Display for FreshnessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimestampSkew => write!(f, "timestamp skew exceeds maximum allowed"),
            Self::Replay => write!(f, "freshness challenge replay detected"),
        }
    }
}

impl std::error::Error for FreshnessError {}

/// Validates freshness challenges.
///
/// CTA owns this trait object with its state. Different validators
/// can be plugged in based on deployment requirements.
pub trait FreshnessValidator: Send + Sync {
    /// Validate a freshness challenge.
    ///
    /// Returns `Ok(())` if fresh, `Err` if stale/invalid.
    fn validate(
        &self,
        challenge: &FreshnessChallenge,
        now: DateTime<Utc>,
    ) -> Result<(), FreshnessError>;
}

/// Stateless validator - validates Random and Timestamp only.
///
/// Use for simple deployments without replay tracking.
pub struct StatelessFreshnessValidator {
    /// Maximum clock skew allowed for timestamp challenges.
    pub max_timestamp_skew: Duration,
}

impl Default for StatelessFreshnessValidator {
    fn default() -> Self {
        Self {
            max_timestamp_skew: Duration::seconds(60),
        }
    }
}

impl StatelessFreshnessValidator {
    /// Create a new stateless validator with custom max skew.
    #[must_use]
    pub fn with_max_skew(max_skew: Duration) -> Self {
        Self {
            max_timestamp_skew: max_skew,
        }
    }
}

impl FreshnessValidator for StatelessFreshnessValidator {
    #[allow(clippy::cast_sign_loss)] // timestamp_millis is always positive after 1970
    fn validate(
        &self,
        challenge: &FreshnessChallenge,
        now: DateTime<Utc>,
    ) -> Result<(), FreshnessError> {
        match challenge {
            // Random: always valid (freshness from nonce in signature)
            FreshnessChallenge::Random(_) => Ok(()),

            // Timestamp: check clock skew
            FreshnessChallenge::Timestamp {
                unix_millis,
                max_skew_ms,
            } => {
                let now_ms = now.timestamp_millis() as u64;
                let our_max_ms = self.max_timestamp_skew.num_milliseconds().unsigned_abs();
                let allowed_skew = our_max_ms.min(u64::from(*max_skew_ms));
                if now_ms.abs_diff(*unix_millis) > allowed_skew {
                    return Err(FreshnessError::TimestampSkew);
                }
                Ok(())
            }
        }
    }
}

/// Stateful validator with in-memory replay detection.
///
/// Tracks Random nonces in a `HashSet`.
/// Timestamp challenges are only skew-checked (not replay-tracked).
pub struct StatefulFreshnessValidator {
    /// Maximum clock skew allowed for timestamp challenges.
    pub max_timestamp_skew: Duration,
    seen_random: Mutex<HashSet<[u8; 32]>>,
}

impl Default for StatefulFreshnessValidator {
    fn default() -> Self {
        Self {
            max_timestamp_skew: Duration::seconds(60),
            seen_random: Mutex::new(HashSet::new()),
        }
    }
}

impl StatefulFreshnessValidator {
    /// Create a new stateful validator with custom max skew.
    #[must_use]
    pub fn with_max_skew(max_skew: Duration) -> Self {
        Self {
            max_timestamp_skew: max_skew,
            ..Default::default()
        }
    }
}

impl FreshnessValidator for StatefulFreshnessValidator {
    #[allow(clippy::cast_sign_loss)] // timestamp_millis is always positive after 1970
    fn validate(
        &self,
        challenge: &FreshnessChallenge,
        now: DateTime<Utc>,
    ) -> Result<(), FreshnessError> {
        match challenge {
            FreshnessChallenge::Random(nonce) => {
                let mut seen = self.seen_random.lock().expect("poisoned mutex");
                if !seen.insert(*nonce) {
                    return Err(FreshnessError::Replay);
                }
                Ok(())
            }
            FreshnessChallenge::Timestamp {
                unix_millis,
                max_skew_ms,
            } => {
                // Use same u64-based approach as StatelessFreshnessValidator
                let now_ms = now.timestamp_millis() as u64;
                let our_max_ms = self.max_timestamp_skew.num_milliseconds().unsigned_abs();
                let allowed_skew = our_max_ms.min(u64::from(*max_skew_ms));
                if now_ms.abs_diff(*unix_millis) > allowed_skew {
                    return Err(FreshnessError::TimestampSkew);
                }
                Ok(())
            }
        }
    }
}

/// Permissive validator - accepts all challenges.
///
/// Use for testing only.
pub struct PermissiveFreshnessValidator;

impl FreshnessValidator for PermissiveFreshnessValidator {
    fn validate(&self, _: &FreshnessChallenge, _: DateTime<Utc>) -> Result<(), FreshnessError> {
        Ok(())
    }
}

// ============================================================================
// Executor Resolution
// ============================================================================

/// Verifies that an executor satisfies a characteristic or is authorized for a CTA reference.
///
/// Single responsibility: given (executor, designation, proof) -> bool.
/// Does NOT resolve designations to keys.
pub trait ExecutorResolver: Send + Sync {
    /// Verify that an executor satisfies a characteristic.
    ///
    /// - `executor`: The public key claiming to satisfy the characteristic
    /// - `characteristic`: The required characteristic from parent PCA
    /// - `proof`: Optional proof bytes (attestation, ZK proof, etc.)
    ///
    /// Returns true if the executor satisfies the characteristic.
    fn verify(
        &self,
        executor: &PublicKey,
        characteristic: &ExecutorCharacteristic,
        proof: Option<&[u8]>,
    ) -> bool;

    /// Verify that an executor is authorized for a CTA reference designation.
    ///
    /// When the parent PCA designates this CTA via `CtaReference`, the CTA has
    /// discretion over which executors can continue the chain. This method
    /// implements that discretion.
    ///
    /// - `executor`: The public key claiming to be authorized
    /// - `cta_ref`: The CTA reference from parent PCA
    /// - `proof`: Optional proof bytes (attestation, authorization token, etc.)
    ///
    /// Returns true if the executor is authorized by this CTA.
    ///
    /// Default implementation rejects all - CTAs must explicitly opt-in to
    /// handling CTA references.
    fn verify_cta_reference(
        &self,
        _executor: &PublicKey,
        _cta_ref: &CtaReference,
        _proof: Option<&[u8]>,
    ) -> bool {
        false // Default: reject all CTA reference designations
    }
}

/// No-op resolver that rejects all characteristic designations.
///
/// Use when CTA only handles direct pubkey designations.
pub struct RejectAllResolver;

impl ExecutorResolver for RejectAllResolver {
    fn verify(&self, _: &PublicKey, _: &ExecutorCharacteristic, _: Option<&[u8]>) -> bool {
        false
    }
}

/// Permissive resolver that accepts all characteristic and CTA reference claims.
///
/// Use for testing only.
pub struct PermissiveResolver;

impl ExecutorResolver for PermissiveResolver {
    fn verify(&self, _: &PublicKey, _: &ExecutorCharacteristic, _: Option<&[u8]>) -> bool {
        true
    }

    fn verify_cta_reference(&self, _: &PublicKey, _: &CtaReference, _: Option<&[u8]>) -> bool {
        true
    }
}

// ============================================================================
// Proof of Continuity
// ============================================================================

/// Proof that the submitter is the designated next executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofOfContinuity {
    /// Hash of parent PCA (commitment).
    pub parent_hash: PcaHash,

    /// Executor's public key.
    pub executor_key: PublicKey,

    /// Signature over: `POC_DOMAIN_SEPARATOR || parent_hash || challenge || request_cbor || char_proof`
    pub signature: Signature,

    /// Freshness challenge (PIC Causal Challenge).
    pub challenge: FreshnessChallenge,

    /// Optional: Proof of characteristic (for characteristic-based designation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub characteristic_proof: Option<Vec<u8>>,
}

impl ProofOfContinuity {
    /// Build a proof of continuity.
    ///
    /// Called by the executor before submitting to CTA.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent PCA cannot be hashed.
    pub fn build(
        parent: &Pca,
        executor: &KeyPair,
        request: &ContinuationRequest,
        challenge: FreshnessChallenge,
    ) -> Result<Self, Error> {
        Self::build_internal(parent, executor, request, challenge, None)
    }

    /// Build a proof of continuity with characteristic proof.
    ///
    /// Called when the executor needs to prove they satisfy a characteristic.
    ///
    /// # Errors
    ///
    /// Returns an error if the parent PCA cannot be hashed.
    pub fn build_with_characteristic(
        parent: &Pca,
        executor: &KeyPair,
        request: &ContinuationRequest,
        challenge: FreshnessChallenge,
        characteristic_proof: Vec<u8>,
    ) -> Result<Self, Error> {
        Self::build_internal(
            parent,
            executor,
            request,
            challenge,
            Some(characteristic_proof),
        )
    }

    fn build_internal(
        parent: &Pca,
        executor: &KeyPair,
        request: &ContinuationRequest,
        challenge: FreshnessChallenge,
        characteristic_proof: Option<Vec<u8>>,
    ) -> Result<Self, Error> {
        let parent_hash = parent.try_hash()?;

        // Build partial PoC to compute message
        let poc_partial = Self {
            parent_hash,
            executor_key: executor.public_key(),
            signature: Signature::placeholder(), // Will be replaced
            challenge: challenge.clone(),
            characteristic_proof: characteristic_proof.clone(),
        };

        let message = poc_partial.build_message(request);
        let signature = executor.sign(&message);

        Ok(Self {
            parent_hash,
            executor_key: executor.public_key(),
            signature,
            challenge,
            characteristic_proof,
        })
    }

    /// Build the message to be signed/verified.
    ///
    /// Includes `characteristic_proof` in signature to prevent proof reuse attacks.
    #[must_use]
    pub fn build_message(&self, request: &ContinuationRequest) -> Vec<u8> {
        let request_cbor =
            canonical_cbor_encode(request).expect("ContinuationRequest must be serializable");

        let mut message = Vec::new();
        message.extend_from_slice(POC_DOMAIN_SEPARATOR);
        message.extend_from_slice(self.parent_hash.as_bytes());
        message.extend_from_slice(&self.challenge.to_bytes());
        message.extend_from_slice(&request_cbor);

        // Bind characteristic_proof to signature (empty if None)
        if let Some(ref proof) = self.characteristic_proof {
            message.extend_from_slice(proof);
        }

        message
    }

    /// Verify the `PoC` signature.
    ///
    /// # Errors
    ///
    /// Returns an error if signature verification fails.
    pub fn verify_signature(&self, request: &ContinuationRequest) -> Result<(), Error> {
        let message = self.build_message(request);
        self.executor_key.verify(&message, &self.signature)
    }
}

// ============================================================================
// Continuation Request
// ============================================================================

/// Request for the next hop in the transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinuationRequest {
    /// Capabilities for the next hop (must be subset of parent, matched by key).
    pub capabilities: Vec<CapabilityData>,

    /// Who can continue after this hop.
    pub designated_executor: DesignatedExecutor,

    /// Expiry (must be <= parent expiry).
    pub expires_at: DateTime<Utc>,

    /// Optional: Additional payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Vec<u8>>,
}

// ============================================================================
// CTA Submission
// ============================================================================

/// A submission to the CTA requesting continuation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtaSubmission {
    /// The PCA being continued (parent).
    pub parent_pca: Pca,

    /// Proof that the submitter is the designated executor.
    pub proof_of_continuity: ProofOfContinuity,

    /// Request for the next hop.
    pub continuation_request: ContinuationRequest,
}

// ============================================================================
// CTA Error
// ============================================================================

/// Errors from CTA operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CtaError {
    /// Root PCA issuer not trusted.
    UntrustedRootAuthority(String),

    /// Child PCA issuer not trusted (expected CTA signer).
    UntrustedCta(String),

    /// Parent PCA has expired.
    ParentExpired,

    /// Freshness challenge failed.
    FreshnessError(FreshnessError),

    /// Parent hash mismatch in proof of continuity.
    HashMismatch,

    /// Executor does not match parent's designated executor.
    ExecutorMismatch,

    /// Executor does not satisfy required characteristic.
    CharacteristicNotSatisfied,

    /// Executor not authorized for CTA reference designation.
    CtaReferenceNotAuthorized,

    /// Submission sent to wrong CTA.
    WrongCta,

    /// Child expiry exceeds parent expiry.
    ExpiryExceedsParent,

    /// Unknown capability key.
    UnknownCapabilityKey(String),

    /// Duplicate capability key.
    DuplicateCapabilityKey(String),

    /// Invalid capability transition.
    InvalidTransition(TransitionError),

    /// Signature verification failed.
    SignatureError(String),

    /// PCA error.
    PcaError(String),
}

impl std::fmt::Display for CtaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UntrustedRootAuthority(issuer) => {
                write!(f, "root PCA issuer not trusted: {issuer}")
            }
            Self::UntrustedCta(issuer) => {
                write!(f, "child PCA issuer not trusted CTA: {issuer}")
            }
            Self::ParentExpired => write!(f, "parent PCA has expired"),
            Self::FreshnessError(e) => write!(f, "freshness challenge failed: {e}"),
            Self::HashMismatch => write!(f, "parent hash mismatch in proof of continuity"),
            Self::ExecutorMismatch => {
                write!(f, "executor does not match parent's designated executor")
            }
            Self::CharacteristicNotSatisfied => {
                write!(f, "executor does not satisfy required characteristic")
            }
            Self::CtaReferenceNotAuthorized => {
                write!(f, "executor not authorized for CTA reference designation")
            }
            Self::WrongCta => write!(f, "submission sent to wrong CTA"),
            Self::ExpiryExceedsParent => write!(f, "child expiry exceeds parent expiry"),
            Self::UnknownCapabilityKey(key) => write!(f, "unknown capability key: {key}"),
            Self::DuplicateCapabilityKey(key) => write!(f, "duplicate capability key: {key}"),
            Self::InvalidTransition(e) => write!(f, "invalid capability transition: {e}"),
            Self::SignatureError(msg) => write!(f, "signature verification failed: {msg}"),
            Self::PcaError(msg) => write!(f, "PCA error: {msg}"),
        }
    }
}

impl std::error::Error for CtaError {}

impl From<FreshnessError> for CtaError {
    fn from(e: FreshnessError) -> Self {
        Self::FreshnessError(e)
    }
}

impl From<TransitionError> for CtaError {
    fn from(e: TransitionError) -> Self {
        Self::InvalidTransition(e)
    }
}

impl From<Error> for CtaError {
    fn from(e: Error) -> Self {
        Self::PcaError(e.to_string())
    }
}

// ============================================================================
// CTA
// ============================================================================

/// Causal Transaction Authority - validates continuity and emits PCAs.
pub struct Cta<V, R, F> {
    /// This CTA's signing keypair.
    keypair: KeyPair,

    /// Transition validator for capability semantics.
    validator: V,

    /// Trusted root authorities (root PCA issuers).
    root_authorities: Vec<PublicKey>,

    /// Trusted CTAs (signers for non-root PCAs).
    trusted_ctas: Vec<PublicKey>,

    /// Executor resolver for characteristic-based designations.
    resolver: R,

    /// Freshness validator for challenge verification.
    freshness: F,
}

impl<V, R, F> Cta<V, R, F>
where
    V: TransitionValidator,
    R: ExecutorResolver,
    F: FreshnessValidator,
{
    /// Create a new CTA.
    #[must_use]
    pub fn new(
        keypair: KeyPair,
        validator: V,
        root_authorities: Vec<PublicKey>,
        trusted_ctas: Vec<PublicKey>,
        resolver: R,
        freshness: F,
    ) -> Self {
        Self {
            keypair,
            validator,
            root_authorities,
            trusted_ctas,
            resolver,
            freshness,
        }
    }

    /// Get this CTA's public key.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.keypair.public_key()
    }

    /// Process a submission and emit a new PCA if valid.
    ///
    /// # Errors
    ///
    /// Returns a `CtaError` if validation fails.
    pub fn submit(
        &self,
        parent: &Pca,
        poc: &ProofOfContinuity,
        request: &ContinuationRequest,
        now: DateTime<Utc>,
    ) -> Result<Pca, CtaError> {
        // 1. Verify parent PCA signature
        parent
            .try_verify_signature()
            .map_err(|e| CtaError::SignatureError(e.to_string()))?;

        // 2. Verify parent issuer is trusted for its position in the chain.
        if parent.is_root() {
            if !self.root_authorities.contains(parent.issuer()) {
                return Err(CtaError::UntrustedRootAuthority(parent.issuer().to_hex()));
            }
        } else if !self.trusted_ctas.contains(parent.issuer()) {
            return Err(CtaError::UntrustedCta(parent.issuer().to_hex()));
        }

        // 3. Verify parent not expired
        if parent.is_expired_at(now) {
            return Err(CtaError::ParentExpired);
        }

        // 4. Verify freshness challenge
        self.freshness.validate(&poc.challenge, now)?;

        // 5. Verify PoC: executor matches parent's designated_executor
        self.verify_proof_of_continuity(parent, poc, request)?;

        // 6. Verify capabilities attenuate using keyed matching
        self.verify_capability_attenuation(parent, request)?;

        // 7. Verify expiry doesn't exceed parent
        if request.expires_at > parent.expires_at() {
            return Err(CtaError::ExpiryExceedsParent);
        }

        // 8. Build and sign new PCA
        let root_hash = match parent.root_hash() {
            Some(root) => *root,
            None => {
                if parent.is_root() {
                    parent.try_hash()?
                } else {
                    return Err(CtaError::PcaError(
                        "non-root parent is missing root_hash".to_string(),
                    ));
                }
            }
        };

        let mut builder = PcaBuilder::new()
            .version(parent.version())
            .designated_executor(request.designated_executor.clone())
            .prev_hash(poc.parent_hash)
            // Preserve the root transaction hash so every hop is self-describing.
            .root_hash(root_hash)
            .expires_at(request.expires_at);

        for cap in &request.capabilities {
            builder = builder.add_capability(cap.clone());
        }

        if let Some(payload) = &request.payload {
            builder = builder.payload_bytes(payload.clone());
        }

        let new_pca = builder.build_and_sign(&self.keypair)?;

        Ok(new_pca)
    }

    /// Process a submission struct (convenience method).
    ///
    /// # Errors
    ///
    /// Returns a `CtaError` if validation fails.
    pub fn submit_request(
        &self,
        submission: &CtaSubmission,
        now: DateTime<Utc>,
    ) -> Result<Pca, CtaError> {
        self.submit(
            &submission.parent_pca,
            &submission.proof_of_continuity,
            &submission.continuation_request,
            now,
        )
    }

    fn verify_proof_of_continuity(
        &self,
        parent: &Pca,
        poc: &ProofOfContinuity,
        request: &ContinuationRequest,
    ) -> Result<(), CtaError> {
        // Verify hash commitment
        let parent_hash = parent.try_hash()?;
        if poc.parent_hash != parent_hash {
            return Err(CtaError::HashMismatch);
        }

        // Verify executor matches designation
        match parent.designated_executor() {
            DesignatedExecutor::PublicKey(pk) => {
                if &poc.executor_key != pk {
                    return Err(CtaError::ExecutorMismatch);
                }
            }
            DesignatedExecutor::Characteristic(char) => {
                if !self.resolver.verify(
                    &poc.executor_key,
                    char,
                    poc.characteristic_proof.as_deref(),
                ) {
                    return Err(CtaError::CharacteristicNotSatisfied);
                }
            }
            DesignatedExecutor::CtaReference(cta_ref) => {
                // This CTA must be the referenced CTA
                if cta_ref.cta_key != self.keypair.public_key() {
                    return Err(CtaError::WrongCta);
                }
                // CTA must authorize the executor via its resolver
                if !self.resolver.verify_cta_reference(
                    &poc.executor_key,
                    cta_ref,
                    poc.characteristic_proof.as_deref(),
                ) {
                    return Err(CtaError::CtaReferenceNotAuthorized);
                }
            }
        }

        // Verify PoC signature with domain separator
        poc.verify_signature(request)
            .map_err(|e| CtaError::SignatureError(e.to_string()))?;

        Ok(())
    }

    fn verify_capability_attenuation(
        &self,
        parent: &Pca,
        request: &ContinuationRequest,
    ) -> Result<(), CtaError> {
        // Build parent capability map by key with duplicate detection.
        let mut parent_caps: HashMap<&str, &CapabilityData> =
            HashMap::with_capacity(parent.capabilities().len());
        for cap in parent.capabilities() {
            if parent_caps.insert(cap.key(), cap).is_some() {
                return Err(CtaError::DuplicateCapabilityKey(cap.key().to_string()));
            }
        }

        // Each child capability must have matching parent (keyed matching)
        let mut seen_keys = std::collections::HashSet::with_capacity(request.capabilities.len());
        for child_cap in &request.capabilities {
            if !seen_keys.insert(child_cap.key()) {
                return Err(CtaError::DuplicateCapabilityKey(
                    child_cap.key().to_string(),
                ));
            }

            let parent_cap = parent_caps
                .get(child_cap.key())
                .ok_or_else(|| CtaError::UnknownCapabilityKey(child_cap.key().to_string()))?;

            // Validate transition (attenuation) using validator
            self.validator.validate_transition(parent_cap, child_cap)?;
        }

        Ok(())
    }
}

// ============================================================================
// CTA Builder
// ============================================================================

/// Builder for creating a CTA with sensible defaults.
pub struct CtaBuilder<
    V = crate::PermissiveValidator,
    R = RejectAllResolver,
    F = PermissiveFreshnessValidator,
> {
    keypair: KeyPair,
    validator: V,
    root_authorities: Vec<PublicKey>,
    trusted_ctas: Vec<PublicKey>,
    resolver: R,
    freshness: F,
}

impl CtaBuilder {
    /// Create a new CTA builder with default validators.
    ///
    /// Note: defaults are permissive and intended for tests or prototypes.
    /// Production deployments should set explicit validators and freshness
    /// policies via `validator(...)` and `freshness(...)`.
    #[must_use]
    pub fn new(keypair: KeyPair) -> Self {
        Self {
            keypair,
            validator: crate::PermissiveValidator,
            root_authorities: Vec::new(),
            trusted_ctas: Vec::new(),
            resolver: RejectAllResolver,
            freshness: PermissiveFreshnessValidator,
        }
    }
}

impl<V, R, F> CtaBuilder<V, R, F> {
    /// Set a custom transition validator.
    #[must_use]
    pub fn validator<V2: TransitionValidator>(self, validator: V2) -> CtaBuilder<V2, R, F> {
        CtaBuilder {
            keypair: self.keypair,
            validator,
            root_authorities: self.root_authorities,
            trusted_ctas: self.trusted_ctas,
            resolver: self.resolver,
            freshness: self.freshness,
        }
    }

    /// Set a custom executor resolver.
    #[must_use]
    pub fn resolver<R2: ExecutorResolver>(self, resolver: R2) -> CtaBuilder<V, R2, F> {
        CtaBuilder {
            keypair: self.keypair,
            validator: self.validator,
            root_authorities: self.root_authorities,
            trusted_ctas: self.trusted_ctas,
            resolver,
            freshness: self.freshness,
        }
    }

    /// Set a custom freshness validator.
    #[must_use]
    pub fn freshness<F2: FreshnessValidator>(self, freshness: F2) -> CtaBuilder<V, R, F2> {
        CtaBuilder {
            keypair: self.keypair,
            validator: self.validator,
            root_authorities: self.root_authorities,
            trusted_ctas: self.trusted_ctas,
            resolver: self.resolver,
            freshness,
        }
    }

    /// Add a trusted root authority.
    #[must_use]
    pub fn trust_root_authority(mut self, issuer: PublicKey) -> Self {
        self.root_authorities.push(issuer);
        self
    }

    /// Add multiple trusted root authorities.
    #[must_use]
    pub fn trust_root_authorities(mut self, issuers: impl IntoIterator<Item = PublicKey>) -> Self {
        self.root_authorities.extend(issuers);
        self
    }

    /// Add a trusted CTA signer for non-root PCAs.
    #[must_use]
    pub fn trust_cta(mut self, cta: PublicKey) -> Self {
        self.trusted_ctas.push(cta);
        self
    }

    /// Add multiple trusted CTAs.
    #[must_use]
    pub fn trust_ctas(mut self, ctas: impl IntoIterator<Item = PublicKey>) -> Self {
        self.trusted_ctas.extend(ctas);
        self
    }

    /// Build the CTA.
    #[must_use]
    pub fn build(self) -> Cta<V, R, F>
    where
        V: TransitionValidator,
        R: ExecutorResolver,
        F: FreshnessValidator,
    {
        Cta::new(
            self.keypair,
            self.validator,
            self.root_authorities,
            self.trusted_ctas,
            self.resolver,
            self.freshness,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Algorithm, PROTOCOL_VERSION, PermissiveValidator};
    use chrono::Duration;

    // Test seeds for deterministic key generation
    const SEED_GATEWAY: [u8; 32] = [1u8; 32];
    const SEED_CTA: [u8; 32] = [2u8; 32];
    const SEED_AGENT: [u8; 32] = [3u8; 32];
    const SEED_NEXT_AGENT: [u8; 32] = [4u8; 32];
    const SEED_WRONG_AGENT: [u8; 32] = [5u8; 32];
    const SEED_UNTRUSTED: [u8; 32] = [6u8; 32];
    const SEED_ISSUER1: [u8; 32] = [7u8; 32];
    const SEED_ISSUER2: [u8; 32] = [8u8; 32];
    const SEED_CLAIMS: [u8; 32] = [9u8; 32];
    const SEED_PAYOUT: [u8; 32] = [10u8; 32];
    const SEED_TERMINAL: [u8; 32] = [11u8; 32];

    // Test nonces for freshness challenges
    const NONCE_1: [u8; 32] = [0x11u8; 32];
    const NONCE_2: [u8; 32] = [0x22u8; 32];

    fn create_test_cap(key: &str) -> CapabilityData {
        CapabilityData::from_json(
            key,
            "function",
            &serde_json::json!({"name": "test.function"}),
        )
        .unwrap()
    }

    #[test]
    fn test_freshness_challenge_from_bytes() {
        let c1 = FreshnessChallenge::from_bytes(NONCE_1);
        let c2 = FreshnessChallenge::from_bytes(NONCE_2);

        // Different nonces should create different challenges
        assert_ne!(c1, c2);

        // Same nonce should create same challenge (deterministic)
        let c1_again = FreshnessChallenge::from_bytes(NONCE_1);
        assert_eq!(c1, c1_again);

        // Should serialize/deserialize
        let bytes = c1.to_bytes();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn test_freshness_challenge_timestamp_at_millis() {
        let c = FreshnessChallenge::timestamp_at_millis(
            1_700_000_000_000,
            Duration::milliseconds(5000),
        );
        match c {
            FreshnessChallenge::Timestamp {
                unix_millis,
                max_skew_ms,
            } => {
                assert_eq!(unix_millis, 1_700_000_000_000);
                assert_eq!(max_skew_ms, 5000);
            }
            _ => panic!("Expected Timestamp variant"),
        }
    }

    #[test]
    fn test_stateless_freshness_validator_random() {
        let validator = StatelessFreshnessValidator::default();
        let challenge = FreshnessChallenge::from_bytes(NONCE_1);

        assert!(validator.validate(&challenge, Utc::now()).is_ok());
    }

    #[test]
    #[allow(clippy::cast_sign_loss)] // timestamp_millis is always positive after 1970
    fn test_stateless_freshness_validator_timestamp_valid() {
        let validator = StatelessFreshnessValidator::default();
        let now = Utc::now();
        let challenge = FreshnessChallenge::Timestamp {
            unix_millis: now.timestamp_millis() as u64,
            max_skew_ms: 5000,
        };

        assert!(validator.validate(&challenge, now).is_ok());
    }

    #[test]
    #[allow(clippy::cast_sign_loss)] // timestamp_millis is always positive after 1970
    fn test_stateless_freshness_validator_timestamp_skew() {
        let validator = StatelessFreshnessValidator::with_max_skew(Duration::seconds(1));
        let now = Utc::now();
        // Challenge from 10 seconds ago
        let challenge = FreshnessChallenge::Timestamp {
            unix_millis: (now.timestamp_millis() - 10_000) as u64,
            max_skew_ms: 500,
        };

        assert!(matches!(
            validator.validate(&challenge, now),
            Err(FreshnessError::TimestampSkew)
        ));
    }

    #[test]
    fn test_stateful_freshness_validator_replay_rejected() {
        let validator = StatefulFreshnessValidator::default();
        let challenge = FreshnessChallenge::from_bytes(NONCE_1);

        assert!(validator.validate(&challenge, Utc::now()).is_ok());
        assert!(matches!(
            validator.validate(&challenge, Utc::now()),
            Err(FreshnessError::Replay)
        ));
    }

    #[test]
    fn test_stateful_freshness_validator_concurrent_replay_detection() {
        use std::sync::Arc;
        use std::thread;

        let validator = Arc::new(StatefulFreshnessValidator::default());
        let challenge = FreshnessChallenge::from_bytes(NONCE_1);
        let mut handles = vec![];

        // Spawn 10 threads all trying to validate the same challenge
        for _ in 0..10 {
            let v = Arc::clone(&validator);
            let c = challenge.clone();
            handles.push(thread::spawn(move || v.validate(&c, Utc::now())));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let successes = results.iter().filter(|r| r.is_ok()).count();
        let replays = results
            .iter()
            .filter(|r| matches!(r, Err(FreshnessError::Replay)))
            .count();

        // Exactly one thread should succeed, the rest should detect replay
        assert_eq!(successes, 1, "Only first validation should succeed");
        assert_eq!(replays, 9, "All other threads should detect replay");
    }

    #[test]
    fn test_permissive_freshness_validator() {
        let validator = PermissiveFreshnessValidator;

        // Should accept all challenges
        assert!(
            validator
                .validate(&FreshnessChallenge::from_bytes(NONCE_1), Utc::now())
                .is_ok()
        );
    }

    #[test]
    fn test_proof_of_continuity_build() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);
        let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_NEXT_AGENT);

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        let parent = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let challenge = FreshnessChallenge::from_bytes(NONCE_1);
        let poc = ProofOfContinuity::build(&parent, &agent, &request, challenge).unwrap();

        // Should be able to verify
        assert!(poc.verify_signature(&request).is_ok());
        assert_eq!(poc.executor_key, agent.public_key());
        assert_eq!(poc.parent_hash, parent.try_hash().unwrap());
    }

    #[test]
    fn test_proof_of_continuity_wrong_key() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);
        let wrong_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_WRONG_AGENT);
        let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_NEXT_AGENT);

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        let parent = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        // Sign with wrong key
        let challenge = FreshnessChallenge::from_bytes(NONCE_1);
        let poc = ProofOfContinuity::build(&parent, &wrong_agent, &request, challenge).unwrap();

        // Signature should verify against the key it was signed with
        assert!(poc.verify_signature(&request).is_ok());

        // But CTA will reject because executor_key != parent.designated_executor
        assert_eq!(poc.executor_key, wrong_agent.public_key());
        assert_ne!(poc.executor_key, agent.public_key());
    }

    #[test]
    fn test_cta_submit_valid() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let claims_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CLAIMS);
        let payout_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_PAYOUT);

        let cta = CtaBuilder::new(cta_keypair.clone())
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:claims");
        let expires = Utc::now() + Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(claims_agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(payout_agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let challenge = FreshnessChallenge::from_bytes(NONCE_1);
        let poc = ProofOfContinuity::build(&root_pca, &claims_agent, &request, challenge).unwrap();

        let child_pca = cta.submit(&root_pca, &poc, &request, Utc::now()).unwrap();

        // Child PCA should be valid
        assert!(child_pca.try_verify_signature().is_ok());
        assert_eq!(child_pca.issuer(), &cta_keypair.public_key());
        assert_eq!(
            child_pca.designated_executor().as_public_key(),
            Some(&payout_agent.public_key())
        );
        assert_eq!(child_pca.prev_hash(), Some(&root_pca.try_hash().unwrap()));
    }

    #[test]
    fn test_cta_submit_untrusted_issuer() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let untrusted = KeyPair::from_seed(Algorithm::Ed25519, &SEED_UNTRUSTED);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        // CTA trusts gateway but not untrusted
        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        // Root signed by untrusted
        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&untrusted)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::UntrustedRootAuthority(_))));
    }

    #[test]
    fn test_cta_submit_expired_parent() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:test");
        let expired = Utc::now() - Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expired)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expired,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::ParentExpired)));
    }

    #[test]
    fn test_cta_submit_executor_mismatch() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);
        let wrong_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_WRONG_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        // Parent designates agent
        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        // PoC signed by wrong_agent
        let poc = ProofOfContinuity::build(
            &root_pca,
            &wrong_agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::ExecutorMismatch)));
    }

    #[test]
    fn test_cta_submit_expiry_exceeds_parent() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:test");
        let parent_expires = Utc::now() + Duration::hours(1);
        let child_expires = Utc::now() + Duration::hours(2); // Exceeds parent!

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(parent_expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: child_expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::ExpiryExceedsParent)));
    }

    #[test]
    fn test_cta_submit_unknown_capability() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let parent_cap = create_test_cap("cap:parent");
        let child_cap = create_test_cap("cap:unknown"); // Not in parent!
        let expires = Utc::now() + Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(parent_cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![child_cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::UnknownCapabilityKey(_))));
    }

    #[test]
    fn test_cta_submit_duplicate_child_capability_keys() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .build();

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap.clone(), cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::DuplicateCapabilityKey(_))));
    }

    #[test]
    fn test_cta_submit_replay_rejected() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);
        let next_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_NEXT_AGENT);

        let cta = CtaBuilder::new(cta_keypair)
            .trust_root_authority(gateway.public_key())
            .freshness(StatefulFreshnessValidator::default())
            .build();

        let cap = create_test_cap("cap:test");
        let expires = Utc::now() + Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(next_agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let challenge = FreshnessChallenge::from_bytes(NONCE_1);
        let poc = ProofOfContinuity::build(&root_pca, &agent, &request, challenge).unwrap();

        let first = cta.submit(&root_pca, &poc, &request, Utc::now());
        assert!(first.is_ok(), "First submission should succeed: {first:?}");

        let replay = cta.submit(&root_pca, &poc, &request, Utc::now());
        assert!(
            matches!(
                replay,
                Err(CtaError::FreshnessError(FreshnessError::Replay))
            ),
            "Replay should be rejected: {replay:?}"
        );
    }

    #[test]
    fn test_cta_submit_with_custom_validator() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);

        // Use StrictValidator which rejects any changes
        let cta = CtaBuilder::new(cta_keypair)
            .validator(crate::StrictValidator)
            .trust_root_authority(gateway.public_key())
            .build();

        let parent_cap = CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "parent"}),
        )
        .unwrap();
        let child_cap = CapabilityData::from_json(
            "cap:test",
            "function",
            &serde_json::json!({"name": "child"}), // Different!
        )
        .unwrap();
        let expires = Utc::now() + Duration::hours(1);

        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(parent_cap)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let request = ContinuationRequest {
            capabilities: vec![child_cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: expires,
            payload: None,
        };

        let poc = ProofOfContinuity::build(
            &root_pca,
            &agent,
            &request,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();

        let result = cta.submit(&root_pca, &poc, &request, Utc::now());

        assert!(matches!(result, Err(CtaError::InvalidTransition(_))));
    }

    #[test]
    fn test_cta_builder() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let issuer1 = KeyPair::from_seed(Algorithm::Ed25519, &SEED_ISSUER1);
        let issuer2 = KeyPair::from_seed(Algorithm::Ed25519, &SEED_ISSUER2);

        let cta = CtaBuilder::new(keypair.clone())
            .trust_root_authority(issuer1.public_key())
            .trust_root_authority(issuer2.public_key())
            .validator(PermissiveValidator)
            .resolver(RejectAllResolver)
            .freshness(PermissiveFreshnessValidator)
            .build();

        assert_eq!(cta.public_key(), keypair.public_key());
    }

    #[test]
    fn test_cta_error_display() {
        let errors = vec![
            CtaError::UntrustedRootAuthority("abc".into()),
            CtaError::UntrustedCta("cta".into()),
            CtaError::ParentExpired,
            CtaError::FreshnessError(FreshnessError::TimestampSkew),
            CtaError::HashMismatch,
            CtaError::ExecutorMismatch,
            CtaError::CharacteristicNotSatisfied,
            CtaError::WrongCta,
            CtaError::ExpiryExceedsParent,
            CtaError::UnknownCapabilityKey("cap:test".into()),
            CtaError::DuplicateCapabilityKey("cap:test".into()),
            CtaError::InvalidTransition(TransitionError::new("test")),
            CtaError::SignatureError("test".into()),
            CtaError::PcaError("test".into()),
        ];

        for err in errors {
            let display = format!("{err}");
            assert!(!display.is_empty());
        }
    }

    #[test]
    fn test_continuation_request_serialization() {
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_AGENT);
        let cap = create_test_cap("cap:test");

        let request = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            expires_at: Utc::now() + Duration::hours(1),
            payload: Some(vec![1, 2, 3]),
        };

        // Should serialize to CBOR
        let cbor = canonical_cbor_encode(&request).unwrap();
        assert!(!cbor.is_empty());

        // Should deserialize back
        let decoded: ContinuationRequest = crate::cbor_decode(&cbor).unwrap();
        assert_eq!(decoded.capabilities.len(), 1);
        assert_eq!(decoded.payload, Some(vec![1, 2, 3]));
    }

    #[test]
    fn test_freshness_challenge_cbor_roundtrip() {
        let challenges = vec![
            FreshnessChallenge::from_bytes(NONCE_1),
            FreshnessChallenge::timestamp_at_millis(
                1_700_000_000_000,
                Duration::milliseconds(5000),
            ),
        ];

        for challenge in challenges {
            let cbor = canonical_cbor_encode(&challenge).unwrap();
            let decoded: FreshnessChallenge = crate::cbor_decode(&cbor).unwrap();
            assert_eq!(challenge, decoded);
        }
    }

    #[test]
    fn test_three_hop_chain_via_cta() {
        // Gateway -> Claims Agent -> Payout Agent -> Terminal
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &SEED_GATEWAY);
        let cta_keypair = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CTA);
        let claims_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_CLAIMS);
        let payout_agent = KeyPair::from_seed(Algorithm::Ed25519, &SEED_PAYOUT);
        let terminal = KeyPair::from_seed(Algorithm::Ed25519, &SEED_TERMINAL);

        let cta = CtaBuilder::new(cta_keypair.clone())
            .trust_root_authority(gateway.public_key())
            .trust_cta(cta_keypair.public_key()) // Trust itself for chain
            .build();

        let cap = create_test_cap("cap:claims");
        let expires = Utc::now() + Duration::hours(1);

        // Hop 1: Gateway -> Claims Agent
        let root_pca = PcaBuilder::new()
            .version(PROTOCOL_VERSION)
            .add_capability(cap.clone())
            .designated_executor(claims_agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Hop 2: Claims Agent -> Payout Agent via CTA
        let request1 = ContinuationRequest {
            capabilities: vec![cap.clone()],
            designated_executor: DesignatedExecutor::from_public_key(payout_agent.public_key()),
            expires_at: expires,
            payload: None,
        };
        let poc1 = ProofOfContinuity::build(
            &root_pca,
            &claims_agent,
            &request1,
            FreshnessChallenge::from_bytes(NONCE_1),
        )
        .unwrap();
        let pca1 = cta.submit(&root_pca, &poc1, &request1, Utc::now()).unwrap();

        assert_eq!(pca1.issuer(), &cta_keypair.public_key());
        assert_eq!(
            pca1.designated_executor().as_public_key(),
            Some(&payout_agent.public_key())
        );

        // Hop 3: Payout Agent -> Terminal via CTA
        let request2 = ContinuationRequest {
            capabilities: vec![cap],
            designated_executor: DesignatedExecutor::from_public_key(terminal.public_key()),
            expires_at: expires,
            payload: None,
        };
        let poc2 = ProofOfContinuity::build(
            &pca1,
            &payout_agent,
            &request2,
            FreshnessChallenge::from_bytes(NONCE_2),
        )
        .unwrap();
        let pca2 = cta.submit(&pca1, &poc2, &request2, Utc::now()).unwrap();

        assert_eq!(pca2.issuer(), &cta_keypair.public_key());
        assert_eq!(
            pca2.designated_executor().as_public_key(),
            Some(&terminal.public_key())
        );
        assert_eq!(pca2.prev_hash(), Some(&pca1.try_hash().unwrap()));
    }
}
