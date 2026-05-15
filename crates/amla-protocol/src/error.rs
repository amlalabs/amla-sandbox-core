//! Error types for the Amla protocol.

use thiserror::Error;

/// Errors that can occur in the Amla protocol.
#[derive(Debug, Error)]
pub enum Error {
    /// Invalid key length.
    #[error("invalid key length: expected {expected} bytes, got {got}")]
    InvalidKeyLength {
        /// Expected length in bytes.
        expected: usize,
        /// Actual length in bytes.
        got: usize,
    },

    /// Invalid signature length.
    #[error("invalid signature length: got {0} bytes")]
    InvalidSignatureLength(usize),

    /// Invalid hash length.
    #[error("invalid hash length: expected 32 bytes, got {0}")]
    InvalidHashLength(usize),

    /// Invalid hex string.
    #[error("invalid hex string: {0}")]
    InvalidHex(#[from] hex::FromHexError),

    /// Signature verification failed.
    #[error("signature verification failed")]
    SignatureVerificationFailed,

    /// Invalid algorithm.
    #[error("invalid algorithm: {0}")]
    InvalidAlgorithm(String),

    /// Invalid key format.
    #[error("invalid key format: {0}")]
    InvalidKeyFormat(String),

    /// Algorithm mismatch.
    #[error("algorithm mismatch: expected {expected}, got {got}")]
    AlgorithmMismatch {
        /// Expected algorithm.
        expected: crate::Algorithm,
        /// Actual algorithm.
        got: crate::Algorithm,
    },

    /// Invalid version.
    #[error("invalid version: {0}")]
    InvalidVersion(String),

    /// CBOR serialization error.
    #[error("CBOR serialization error: {0}")]
    CborSerialize(String),

    /// CBOR deserialization error.
    #[error("CBOR deserialization error: {0}")]
    CborDeserialize(String),

    /// Invalid PCA structure.
    #[error("invalid PCA: {0}")]
    InvalidPca(String),

    /// Expired PCA.
    #[error("PCA expired at {0}")]
    PcaExpired(String),

    /// Missing required field.
    #[error("missing required field: {0}")]
    MissingField(String),

    /// Duplicate capability key.
    #[error("duplicate capability key: {0}")]
    DuplicateCapabilityKey(String),

    /// Invalid capability.
    #[error("invalid capability: {0}")]
    InvalidCapability(String),

    /// Capability type mismatch.
    #[error("capability type mismatch: expected {expected}, got {got}")]
    CapabilityTypeMismatch {
        /// Expected capability type.
        expected: String,
        /// Actual capability type.
        got: String,
    },

    /// Payload size exceeds limit.
    #[error("payload size {size} bytes exceeds limit of {limit} bytes")]
    PayloadTooLarge {
        /// Actual size in bytes.
        size: usize,
        /// Maximum allowed size in bytes.
        limit: usize,
    },

    /// Chain depth exceeds limit.
    #[error("chain depth {depth} exceeds limit of {limit}")]
    ChainTooDeep {
        /// Actual chain depth.
        depth: usize,
        /// Maximum allowed depth.
        limit: usize,
    },
}

/// Result type alias for Amla protocol operations.
pub type Result<T> = std::result::Result<T, Error>;
