//! Algorithm-agnostic cryptographic identity and operations.
//!
//! Supports multiple signing algorithms with Ed25519 as the default.
//!
//! # Design Decisions
//!
//! ## Algorithm Abstraction
//!
//! The identity types (`PublicKey`, `PrivateKey`, `Signature`) use an enum-based
//! internal representation (`PublicKeyInner`, etc.) rather than trait objects or
//! generics. This enables:
//! - **Compile-time size knowledge**: Fixed-size types can be stack-allocated
//! - **Exhaustive matching**: Adding a new algorithm is a breaking change (intentional)
//! - **No heap allocation**: Inner enums store bytes directly in fixed arrays
//!
//! ## Wire Format
//!
//! Two serialization formats are supported:
//! - **CBOR**: `[algorithm_u8, bytes]` - Integer discriminator for compactness
//! - **Hex**: `"algorithm:hex_bytes"` - String prefix for human readability
//!
//! The CBOR format uses `#[repr(u8)]` on `Algorithm` to ensure stable wire encoding.
//! Adding new algorithms must preserve existing discriminator values.
//!
//! ## Security Considerations
//!
//! - `PrivateKey::Debug` intentionally masks secret bytes as `***`
//! - `PrivateKey` does not implement `Display` to prevent accidental logging
//! - `PrivateKey` implements `Zeroize` and `Drop` to clear secret bytes from memory
//! - Signature verification checks algorithm match before cryptographic operations

use ed25519_dalek::{Signer, Verifier};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::error::{Error, Result};

/// Cryptographic algorithm identifier.
///
/// Uses `#[repr(u8)]` for compact wire format serialization. The discriminator
/// values are part of the protocol and must remain stable across versions.
///
/// # Adding New Algorithms
///
/// To add a new algorithm (e.g., Secp256k1):
/// 1. Add a variant with the next available discriminator: `Secp256k1 = 1`
/// 2. Update `as_str()`, `parse()`, `from_u8()` with the new mapping
/// 3. Update length methods (`public_key_len()`, etc.) for the new algorithm
/// 4. Add matching arms in `PublicKeyInner`, `PrivateKeyInner`, `SignatureInner`
/// 5. Implement the cryptographic operations in each type's methods
///
/// The design ensures the compiler catches incomplete match arms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Algorithm {
    /// Ed25519 (default) - 32-byte keys, 64-byte signatures.
    ///
    /// Chosen as default for: speed, small keys/signatures, resistance to
    /// side-channel attacks, and deterministic signatures.
    #[default]
    Ed25519 = 0,
    // Future algorithms (discriminator values reserved):
    // Secp256k1 = 1,  // Bitcoin/Ethereum compatibility
    // P256 = 2,       // NIST curve, HSM support
}

impl Algorithm {
    /// Get the algorithm name as a string prefix.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ed25519 => "ed25519",
        }
    }

    /// Parse algorithm from string prefix.
    ///
    /// # Errors
    ///
    /// Returns an error if the algorithm name is not recognized.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "ed25519" => Ok(Self::Ed25519),
            _ => Err(Error::InvalidAlgorithm(s.to_string())),
        }
    }

    /// Get algorithm from integer discriminator.
    ///
    /// # Errors
    ///
    /// Returns an error if the discriminator is not recognized.
    pub fn from_u8(v: u8) -> Result<Self> {
        match v {
            0 => Ok(Self::Ed25519),
            _ => Err(Error::InvalidAlgorithm(format!(
                "unknown algorithm id: {v}"
            ))),
        }
    }

    /// Get the public key length for this algorithm.
    #[must_use]
    pub const fn public_key_len(&self) -> usize {
        match self {
            Self::Ed25519 => 32,
        }
    }

    /// Get the private key length for this algorithm.
    #[must_use]
    pub const fn private_key_len(&self) -> usize {
        match self {
            Self::Ed25519 => 32,
        }
    }

    /// Get the signature length for this algorithm.
    #[must_use]
    pub const fn signature_len(&self) -> usize {
        match self {
            Self::Ed25519 => 64,
        }
    }
}

impl std::fmt::Display for Algorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ============================================================================
// PublicKey
// ============================================================================

/// Internal representation of public key bytes by algorithm.
#[derive(Clone, PartialEq, Eq, Hash)]
enum PublicKeyInner {
    Ed25519([u8; 32]),
}

/// Algorithm-agnostic public key.
///
/// This is the agent's identity - share this with others.
///
/// # Wire Format
///
/// - CBOR: `[algorithm_u8, bytes]` - compact integer discriminator
/// - Hex: `"ed25519:abcdef..."` - human-readable prefix
///
/// # Example
///
/// ```
/// use amla_protocol::{KeyPair, PublicKey, Algorithm};
///
/// let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
/// let public_key = keypair.public_key();
///
/// // Hex format includes algorithm prefix
/// let hex = public_key.to_hex();
/// assert!(hex.starts_with("ed25519:"));
///
/// // Parse back
/// let pk2 = PublicKey::from_hex(&hex).unwrap();
/// assert_eq!(public_key, pk2);
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PublicKey {
    inner: PublicKeyInner,
}

impl PublicKey {
    /// Create from algorithm and raw bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the byte length doesn't match the algorithm.
    pub fn new(algorithm: Algorithm, bytes: &[u8]) -> Result<Self> {
        match algorithm {
            Algorithm::Ed25519 => {
                if bytes.len() != 32 {
                    return Err(Error::InvalidKeyLength {
                        expected: 32,
                        got: bytes.len(),
                    });
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(bytes);
                Ok(Self {
                    inner: PublicKeyInner::Ed25519(arr),
                })
            }
        }
    }

    /// Create Ed25519 public key from 32-byte array.
    #[must_use]
    pub const fn ed25519(bytes: [u8; 32]) -> Self {
        Self {
            inner: PublicKeyInner::Ed25519(bytes),
        }
    }

    /// Create a zero/null public key for testing.
    #[must_use]
    pub const fn zero() -> Self {
        Self::ed25519([0u8; 32])
    }

    /// Get the algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        match self.inner {
            PublicKeyInner::Ed25519(_) => Algorithm::Ed25519,
        }
    }

    /// Get the raw bytes (without algorithm prefix).
    ///
    /// NOTE: The single-arm match is intentional. When new algorithms are added
    /// (e.g., Secp256k1), the compiler will enforce exhaustive matching, ensuring
    /// all code paths handle the new variant.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.inner {
            PublicKeyInner::Ed25519(bytes) => bytes,
        }
    }

    /// Convert to prefixed hex string: `"algorithm:hex_bytes"`.
    #[must_use]
    pub fn to_hex(&self) -> String {
        match &self.inner {
            PublicKeyInner::Ed25519(bytes) => {
                let prefix = self.algorithm().as_str();
                let mut out = String::with_capacity(prefix.len() + 1 + 64);
                out.push_str(prefix);
                out.push(':');
                let mut buf = [0u8; 64];
                hex::encode_to_slice(bytes, &mut buf).expect("buffer size matches");
                out.push_str(std::str::from_utf8(&buf).expect("hex is valid utf-8"));
                out
            }
        }
    }

    /// Create from prefixed hex string.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_hex(s: &str) -> Result<Self> {
        let (algo_str, hex_bytes) = s
            .split_once(':')
            .ok_or_else(|| Error::InvalidKeyFormat("missing algorithm prefix".to_string()))?;

        let algorithm = Algorithm::parse(algo_str)?;
        match algorithm {
            Algorithm::Ed25519 => {
                if hex_bytes.len() % 2 != 0 {
                    return Err(Error::InvalidKeyFormat(
                        "hex string must have even length".to_string(),
                    ));
                }
                if hex_bytes.len() != 64 {
                    return Err(Error::InvalidKeyLength {
                        expected: 32,
                        got: hex_bytes.len() / 2,
                    });
                }
                let mut arr = [0u8; 32];
                hex::decode_to_slice(hex_bytes, &mut arr)?;
                Ok(Self {
                    inner: PublicKeyInner::Ed25519(arr),
                })
            }
        }
    }

    /// Verify a signature against this public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature is invalid or algorithm mismatch.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<()> {
        if self.algorithm() != signature.algorithm() {
            return Err(Error::AlgorithmMismatch {
                expected: self.algorithm(),
                got: signature.algorithm(),
            });
        }

        match (&self.inner, &signature.inner) {
            (PublicKeyInner::Ed25519(pk_bytes), SignatureInner::Ed25519(sig_bytes)) => {
                let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(pk_bytes)
                    .map_err(|_| Error::SignatureVerificationFailed)?;
                let sig = ed25519_dalek::Signature::from_bytes(sig_bytes);
                verifying_key
                    .verify(message, &sig)
                    .map_err(|_| Error::SignatureVerificationFailed)
            }
        }
    }

    /// Serialize to wire format: `[algorithm_u8, bytes]`.
    #[must_use]
    pub fn to_wire(&self) -> (u8, Vec<u8>) {
        (self.algorithm() as u8, self.as_bytes().to_vec())
    }

    /// Deserialize from wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_wire(algorithm: u8, bytes: &[u8]) -> Result<Self> {
        let algo = Algorithm::from_u8(algorithm)?;
        Self::new(algo, bytes)
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            PublicKeyInner::Ed25519(bytes) => {
                let mut buf = [0u8; 16];
                hex::encode_to_slice(&bytes[..8], &mut buf).expect("buffer size matches");
                let preview = std::str::from_utf8(&buf).expect("hex is valid utf-8");
                write!(f, "PublicKey({}:{}...)", self.algorithm(), preview)
            }
        }
    }
}

impl std::fmt::Display for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            PublicKeyInner::Ed25519(bytes) => {
                let prefix = self.algorithm().as_str();
                let mut buf = [0u8; 64];
                hex::encode_to_slice(bytes, &mut buf).map_err(|_| std::fmt::Error)?;
                f.write_str(prefix)?;
                f.write_str(":")?;
                f.write_str(std::str::from_utf8(&buf).map_err(|_| std::fmt::Error)?)?;
                Ok(())
            }
        }
    }
}

impl AsRef<[u8]> for PublicKey {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

// Serde: serialize as [algorithm_u8, bytes] for CBOR compactness
impl Serialize for PublicKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (algo, bytes) = self.to_wire();
        (algo, serde_bytes::Bytes::new(&bytes)).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (algo, bytes): (u8, serde_bytes::ByteBuf) = Deserialize::deserialize(deserializer)?;
        Self::from_wire(algo, &bytes).map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// PrivateKey
// ============================================================================

/// Internal representation of private key bytes by algorithm.
#[derive(Clone)]
enum PrivateKeyInner {
    Ed25519([u8; 32]),
}

/// Algorithm-agnostic private key.
///
/// **WARNING**: Keep this secret! Anyone with the private key can impersonate the identity.
///
/// # Example
///
/// ```
/// use amla_protocol::{KeyPair, Algorithm};
///
/// let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
/// let private_key = keypair.private_key();
///
/// // Store securely, never expose
/// let hex = private_key.to_hex();
/// assert!(hex.starts_with("ed25519:"));
/// ```
#[derive(Clone)]
pub struct PrivateKey {
    inner: PrivateKeyInner,
}

impl PrivateKey {
    /// Create from algorithm and raw bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the byte length doesn't match the algorithm.
    pub fn new(algorithm: Algorithm, bytes: &[u8]) -> Result<Self> {
        match algorithm {
            Algorithm::Ed25519 => {
                if bytes.len() != 32 {
                    return Err(Error::InvalidKeyLength {
                        expected: 32,
                        got: bytes.len(),
                    });
                }
                let mut arr = [0u8; 32];
                arr.copy_from_slice(bytes);
                Ok(Self {
                    inner: PrivateKeyInner::Ed25519(arr),
                })
            }
        }
    }

    /// Create Ed25519 private key from 32-byte array.
    #[must_use]
    pub const fn ed25519(bytes: [u8; 32]) -> Self {
        Self {
            inner: PrivateKeyInner::Ed25519(bytes),
        }
    }

    /// Get the algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        match self.inner {
            PrivateKeyInner::Ed25519(_) => Algorithm::Ed25519,
        }
    }

    /// Get the raw bytes (without algorithm prefix).
    ///
    /// **WARNING**: Handle with care - this is secret key material.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.inner {
            PrivateKeyInner::Ed25519(bytes) => bytes,
        }
    }

    /// Convert to prefixed hex string: `"algorithm:hex_bytes"`.
    ///
    /// **WARNING**: Handle with care - this is secret key material.
    #[must_use]
    pub fn to_hex(&self) -> String {
        match &self.inner {
            PrivateKeyInner::Ed25519(bytes) => {
                let prefix = self.algorithm().as_str();
                let mut out = String::with_capacity(prefix.len() + 1 + 64);
                out.push_str(prefix);
                out.push(':');
                let mut buf = [0u8; 64];
                hex::encode_to_slice(bytes, &mut buf).expect("buffer size matches");
                out.push_str(std::str::from_utf8(&buf).expect("hex is valid utf-8"));
                out
            }
        }
    }

    /// Create from prefixed hex string.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_hex(s: &str) -> Result<Self> {
        let (algo_str, hex_bytes) = s
            .split_once(':')
            .ok_or_else(|| Error::InvalidKeyFormat("missing algorithm prefix".to_string()))?;

        let algorithm = Algorithm::parse(algo_str)?;
        match algorithm {
            Algorithm::Ed25519 => {
                if hex_bytes.len() % 2 != 0 {
                    return Err(Error::InvalidKeyFormat(
                        "hex string must have even length".to_string(),
                    ));
                }
                if hex_bytes.len() != 64 {
                    return Err(Error::InvalidKeyLength {
                        expected: 32,
                        got: hex_bytes.len() / 2,
                    });
                }
                let mut arr = [0u8; 32];
                hex::decode_to_slice(hex_bytes, &mut arr)?;
                Ok(Self {
                    inner: PrivateKeyInner::Ed25519(arr),
                })
            }
        }
    }

    /// Serialize to wire format: `[algorithm_u8, bytes]`.
    #[must_use]
    pub fn to_wire(&self) -> (u8, Vec<u8>) {
        (self.algorithm() as u8, self.as_bytes().to_vec())
    }

    /// Deserialize from wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_wire(algorithm: u8, bytes: &[u8]) -> Result<Self> {
        let algo = Algorithm::from_u8(algorithm)?;
        Self::new(algo, bytes)
    }
}

impl std::fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PrivateKey({}:***)", self.algorithm())
    }
}

impl PartialEq for PrivateKey {
    fn eq(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (PrivateKeyInner::Ed25519(a), PrivateKeyInner::Ed25519(b)) => a == b,
        }
    }
}

impl Eq for PrivateKey {}

// Serde: serialize as [algorithm_u8, bytes] for CBOR compactness
impl Serialize for PrivateKey {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (algo, bytes) = self.to_wire();
        (algo, serde_bytes::Bytes::new(&bytes)).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PrivateKey {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (algo, bytes): (u8, serde_bytes::ByteBuf) = Deserialize::deserialize(deserializer)?;
        Self::from_wire(algo, &bytes).map_err(serde::de::Error::custom)
    }
}

impl Zeroize for PrivateKeyInner {
    fn zeroize(&mut self) {
        match self {
            PrivateKeyInner::Ed25519(bytes) => bytes.zeroize(),
        }
    }
}

impl Zeroize for PrivateKey {
    fn zeroize(&mut self) {
        self.inner.zeroize();
    }
}

impl Drop for PrivateKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}

// ============================================================================
// Signature
// ============================================================================

/// Internal representation of signature bytes by algorithm.
#[derive(Clone, PartialEq, Eq, Hash)]
enum SignatureInner {
    Ed25519([u8; 64]),
}

/// Algorithm-agnostic cryptographic signature.
///
/// # Example
///
/// ```
/// use amla_protocol::{KeyPair, Algorithm};
///
/// let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
/// let signature = keypair.sign(b"hello world");
///
/// // Hex format includes algorithm prefix
/// let hex = signature.to_hex();
/// assert!(hex.starts_with("ed25519:"));
///
/// // Verify
/// keypair.public_key().verify(b"hello world", &signature).unwrap();
/// ```
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Signature {
    inner: SignatureInner,
}

impl Signature {
    /// Create from algorithm and raw bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the byte length doesn't match the algorithm.
    pub fn new(algorithm: Algorithm, bytes: &[u8]) -> Result<Self> {
        match algorithm {
            Algorithm::Ed25519 => {
                if bytes.len() != 64 {
                    return Err(Error::InvalidSignatureLength(bytes.len()));
                }
                let mut arr = [0u8; 64];
                arr.copy_from_slice(bytes);
                Ok(Self {
                    inner: SignatureInner::Ed25519(arr),
                })
            }
        }
    }

    /// Create Ed25519 signature from 64-byte array.
    #[must_use]
    pub const fn ed25519(bytes: [u8; 64]) -> Self {
        Self {
            inner: SignatureInner::Ed25519(bytes),
        }
    }

    /// Create a placeholder signature (all zeros).
    ///
    /// Used when building a message to sign, where the signature
    /// itself will be computed later.
    #[must_use]
    pub const fn placeholder() -> Self {
        Self {
            inner: SignatureInner::Ed25519([0u8; 64]),
        }
    }

    /// Create a zero/null signature for testing.
    #[must_use]
    pub const fn zero() -> Self {
        Self::placeholder()
    }

    /// Get the algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        match self.inner {
            SignatureInner::Ed25519(_) => Algorithm::Ed25519,
        }
    }

    /// Get the raw bytes (without algorithm prefix).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.inner {
            SignatureInner::Ed25519(bytes) => bytes,
        }
    }

    /// Convert to prefixed hex string: `"algorithm:hex_bytes"`.
    #[must_use]
    pub fn to_hex(&self) -> String {
        match &self.inner {
            SignatureInner::Ed25519(bytes) => {
                let prefix = self.algorithm().as_str();
                let mut out = String::with_capacity(prefix.len() + 1 + 128);
                out.push_str(prefix);
                out.push(':');
                let mut buf = [0u8; 128];
                hex::encode_to_slice(bytes, &mut buf).expect("buffer size matches");
                out.push_str(std::str::from_utf8(&buf).expect("hex is valid utf-8"));
                out
            }
        }
    }

    /// Create from prefixed hex string.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_hex(s: &str) -> Result<Self> {
        let (algo_str, hex_bytes) = s
            .split_once(':')
            .ok_or_else(|| Error::InvalidKeyFormat("missing algorithm prefix".to_string()))?;

        let algorithm = Algorithm::parse(algo_str)?;
        match algorithm {
            Algorithm::Ed25519 => {
                if hex_bytes.len() % 2 != 0 {
                    return Err(Error::InvalidKeyFormat(
                        "hex string must have even length".to_string(),
                    ));
                }
                if hex_bytes.len() != 128 {
                    return Err(Error::InvalidSignatureLength(hex_bytes.len() / 2));
                }
                let mut arr = [0u8; 64];
                hex::decode_to_slice(hex_bytes, &mut arr)?;
                Ok(Self {
                    inner: SignatureInner::Ed25519(arr),
                })
            }
        }
    }

    /// Serialize to wire format: `[algorithm_u8, bytes]`.
    #[must_use]
    pub fn to_wire(&self) -> (u8, Vec<u8>) {
        (self.algorithm() as u8, self.as_bytes().to_vec())
    }

    /// Deserialize from wire format.
    ///
    /// # Errors
    ///
    /// Returns an error if the format is invalid.
    pub fn from_wire(algorithm: u8, bytes: &[u8]) -> Result<Self> {
        let algo = Algorithm::from_u8(algorithm)?;
        Self::new(algo, bytes)
    }
}

impl std::fmt::Debug for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            SignatureInner::Ed25519(bytes) => {
                let mut buf = [0u8; 16];
                hex::encode_to_slice(&bytes[..8], &mut buf).expect("buffer size matches");
                let preview = std::str::from_utf8(&buf).expect("hex is valid utf-8");
                write!(f, "Signature({}:{}...)", self.algorithm(), preview)
            }
        }
    }
}

impl AsRef<[u8]> for Signature {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

// Serde: serialize as [algorithm_u8, bytes] for CBOR compactness
impl Serialize for Signature {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let (algo, bytes) = self.to_wire();
        (algo, serde_bytes::Bytes::new(&bytes)).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (algo, bytes): (u8, serde_bytes::ByteBuf) = Deserialize::deserialize(deserializer)?;
        Self::from_wire(algo, &bytes).map_err(serde::de::Error::custom)
    }
}

// ============================================================================
// KeyPair
// ============================================================================

/// Internal representation of signing key by algorithm.
#[derive(Clone)]
enum KeyPairInner {
    Ed25519(ed25519_dalek::SigningKey),
}

/// Algorithm-agnostic keypair for signing and verification.
///
/// Combines private and public key with signing operations.
///
/// # Example
///
/// ```
/// use amla_protocol::{KeyPair, Algorithm};
///
/// // Generate new keypair
/// let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
///
/// // Sign data
/// let signature = keypair.sign(b"some message");
///
/// // Verify signature
/// keypair.public_key().verify(b"some message", &signature).unwrap();
///
/// // Serialize private key for storage
/// let private_key = keypair.private_key();
///
/// // Reconstruct later
/// let keypair2 = KeyPair::from_private_key(&private_key).unwrap();
/// assert_eq!(keypair.public_key(), keypair2.public_key());
/// ```
#[derive(Clone)]
pub struct KeyPair {
    inner: KeyPairInner,
}

impl KeyPair {
    /// Create a keypair deterministically from a 32-byte seed.
    ///
    /// The seed bytes are used directly as the private key material.
    /// This enables deterministic key generation for WASM environments
    /// where the host provides a random seed at initialization.
    ///
    /// # Security
    ///
    /// - The seed MUST be cryptographically random (32 bytes of entropy)
    /// - Never reuse the same seed for different purposes
    /// - Consider using [`Self::from_seed_with_domain`] for additional safety
    ///
    /// # Example
    ///
    /// ```
    /// use amla_protocol::{KeyPair, Algorithm};
    ///
    /// // Host-provided random seed
    /// let seed: [u8; 32] = [0x42; 32]; // In practice, from host's CSPRNG
    ///
    /// let keypair = KeyPair::from_seed(Algorithm::Ed25519, &seed);
    ///
    /// // Same seed always produces same keypair
    /// let keypair2 = KeyPair::from_seed(Algorithm::Ed25519, &seed);
    /// assert_eq!(keypair.public_key(), keypair2.public_key());
    /// ```
    #[must_use]
    pub fn from_seed(algorithm: Algorithm, seed: &[u8; 32]) -> Self {
        match algorithm {
            Algorithm::Ed25519 => {
                let signing_key = ed25519_dalek::SigningKey::from_bytes(seed);
                Self {
                    inner: KeyPairInner::Ed25519(signing_key),
                }
            }
        }
    }

    /// Create a keypair deterministically from a seed with domain separation.
    ///
    /// Uses HKDF-like derivation: `SHA-256(domain || seed)` to derive the
    /// private key. This prevents accidental key reuse when the same seed
    /// is used for different purposes.
    ///
    /// # Domain Separation
    ///
    /// The domain string should uniquely identify the key's purpose:
    /// - `"amla:runtime:agent"` - Agent identity key
    /// - `"amla:runtime:session:123"` - Session-specific key
    /// - `"amla:test:fixture"` - Test fixture key
    ///
    /// # Example
    ///
    /// ```
    /// use amla_protocol::{KeyPair, Algorithm};
    ///
    /// let seed: [u8; 32] = [0x42; 32];
    ///
    /// // Different domains produce different keys from the same seed
    /// let agent_key = KeyPair::from_seed_with_domain(
    ///     Algorithm::Ed25519,
    ///     &seed,
    ///     "amla:runtime:agent"
    /// );
    /// let session_key = KeyPair::from_seed_with_domain(
    ///     Algorithm::Ed25519,
    ///     &seed,
    ///     "amla:runtime:session:42"
    /// );
    ///
    /// assert_ne!(agent_key.public_key(), session_key.public_key());
    /// ```
    #[must_use]
    pub fn from_seed_with_domain(algorithm: Algorithm, seed: &[u8; 32], domain: &str) -> Self {
        use sha2::{Digest, Sha256};

        // Derive key material: SHA-256(domain || seed)
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update(seed);
        let derived: [u8; 32] = hasher.finalize().into();

        Self::from_seed(algorithm, &derived)
    }

    /// Reconstruct keypair from private key.
    ///
    /// # Errors
    ///
    /// Returns an error if the private key is invalid.
    pub fn from_private_key(private_key: &PrivateKey) -> Result<Self> {
        match &private_key.inner {
            PrivateKeyInner::Ed25519(bytes) => {
                let signing_key = ed25519_dalek::SigningKey::from_bytes(bytes);
                Ok(Self {
                    inner: KeyPairInner::Ed25519(signing_key),
                })
            }
        }
    }

    /// Get the algorithm.
    #[must_use]
    pub const fn algorithm(&self) -> Algorithm {
        match self.inner {
            KeyPairInner::Ed25519(_) => Algorithm::Ed25519,
        }
    }

    /// Get the public key.
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        match &self.inner {
            KeyPairInner::Ed25519(signing_key) => {
                PublicKey::ed25519(signing_key.verifying_key().to_bytes())
            }
        }
    }

    /// Get the private key.
    ///
    /// **WARNING**: Keep this secret!
    #[must_use]
    pub fn private_key(&self) -> PrivateKey {
        match &self.inner {
            KeyPairInner::Ed25519(signing_key) => PrivateKey::ed25519(signing_key.to_bytes()),
        }
    }

    /// Get the agent ID (hex-encoded public key with algorithm prefix).
    #[must_use]
    pub fn id(&self) -> String {
        self.public_key().to_hex()
    }

    /// Sign data with this keypair's private key.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> Signature {
        match &self.inner {
            KeyPairInner::Ed25519(signing_key) => {
                let sig = signing_key.sign(message);
                Signature::ed25519(sig.to_bytes())
            }
        }
    }

    /// Verify a signature against this keypair's public key.
    ///
    /// # Errors
    ///
    /// Returns an error if the signature is invalid.
    pub fn verify(&self, message: &[u8], signature: &Signature) -> Result<()> {
        self.public_key().verify(message, signature)
    }
}

impl std::fmt::Debug for KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self.id();
        let prefix_end = id.find(':').map_or(16, |i| i + 1 + 16);
        write!(f, "KeyPair({}...)", &id[..prefix_end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a deterministic keypair from a seed byte (for testing).
    fn keypair(seed: u8) -> KeyPair {
        KeyPair::from_seed(Algorithm::Ed25519, &[seed; 32])
    }

    #[test]
    fn test_keypair_from_seed() {
        let kp = keypair(1);
        assert_eq!(kp.algorithm(), Algorithm::Ed25519);
        assert_eq!(kp.public_key().as_bytes().len(), 32);
        assert_eq!(kp.private_key().as_bytes().len(), 32);
    }

    #[test]
    fn test_sign_and_verify() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"hello world";

        let signature = keypair.sign(message);
        assert!(keypair.verify(message, &signature).is_ok());

        // Wrong message should fail
        assert!(keypair.verify(b"wrong message", &signature).is_err());
    }

    #[test]
    fn test_verify_with_public_key_only() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"test message";
        let signature = keypair.sign(message);

        let public_key = keypair.public_key();
        assert!(public_key.verify(message, &signature).is_ok());
    }

    #[test]
    fn test_reconstruct_from_private_key() {
        let keypair1 = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let private_key = keypair1.private_key();

        let keypair2 = KeyPair::from_private_key(&private_key).unwrap();
        assert_eq!(keypair1.public_key(), keypair2.public_key());

        // Signatures should be deterministic (Ed25519 is deterministic)
        let message = b"test";
        assert_eq!(keypair1.sign(message), keypair2.sign(message));
    }

    #[test]
    fn test_hex_roundtrip_with_prefix() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let private_key = keypair.private_key();
        let public_key = keypair.public_key();

        // Public key
        let pk_hex = public_key.to_hex();
        assert!(pk_hex.starts_with("ed25519:"));
        let pk2 = PublicKey::from_hex(&pk_hex).unwrap();
        assert_eq!(public_key, pk2);

        // Private key
        let sk_hex = private_key.to_hex();
        assert!(sk_hex.starts_with("ed25519:"));
        let sk2 = PrivateKey::from_hex(&sk_hex).unwrap();
        assert_eq!(private_key, sk2);
    }

    #[test]
    fn test_signature_hex_roundtrip() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"test";
        let signature = keypair.sign(message);

        let hex = signature.to_hex();
        assert!(hex.starts_with("ed25519:"));
        // ed25519: (8 chars) + 128 hex chars = 136 total
        assert_eq!(hex.len(), 8 + 128);

        let sig2 = Signature::from_hex(&hex).unwrap();
        assert_eq!(signature, sig2);
        assert!(keypair.verify(message, &sig2).is_ok());
    }

    #[test]
    fn test_invalid_key_length() {
        assert!(PublicKey::new(Algorithm::Ed25519, &[0u8; 31]).is_err());
        assert!(PublicKey::new(Algorithm::Ed25519, &[0u8; 33]).is_err());
        assert!(PrivateKey::new(Algorithm::Ed25519, &[0u8; 31]).is_err());
        assert!(Signature::new(Algorithm::Ed25519, &[0u8; 63]).is_err());
    }

    #[test]
    fn test_private_key_debug_hides_secret() {
        let private_key = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]).private_key();
        let debug_str = format!("{private_key:?}");
        assert!(debug_str.contains("***"));
        assert!(!debug_str.contains(&hex::encode(private_key.as_bytes())));
    }

    #[test]
    fn test_verify_with_wrong_public_key() {
        let keypair1 = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let keypair2 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let message = b"test message";
        let signature = keypair1.sign(message);

        // Verification with wrong public key should fail
        assert!(keypair2.verify(message, &signature).is_err());
        assert!(keypair2.public_key().verify(message, &signature).is_err());
    }

    #[test]
    fn test_verify_with_tampered_signature() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"test message";
        let signature = keypair.sign(message);

        // Tamper with signature
        let mut tampered_bytes = signature.as_bytes().to_vec();
        tampered_bytes[0] ^= 0xFF;
        let tampered_sig = Signature::new(Algorithm::Ed25519, &tampered_bytes).unwrap();

        assert!(keypair.verify(message, &tampered_sig).is_err());
    }

    #[test]
    fn test_empty_message_signing() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"";
        let signature = keypair.sign(message);
        assert!(keypair.verify(message, &signature).is_ok());
    }

    #[test]
    fn test_large_message_signing() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = vec![0xAB; 1024 * 1024]; // 1MB
        let signature = keypair.sign(&message);
        assert!(keypair.verify(&message, &signature).is_ok());
    }

    #[test]
    fn test_invalid_hex_strings() {
        // Missing prefix
        assert!(PublicKey::from_hex("abcdef").is_err());
        // Wrong prefix
        assert!(PublicKey::from_hex("unknown:abcdef").is_err());
        // Too short
        assert!(PublicKey::from_hex("ed25519:abcd").is_err());
        // Invalid hex chars
        assert!(PublicKey::from_hex(&format!("ed25519:{}", "gg".repeat(32))).is_err());

        // Same for PrivateKey
        assert!(PrivateKey::from_hex("abcd").is_err());

        // Same for Signature
        assert!(Signature::from_hex("abcd").is_err());
    }

    #[test]
    fn test_public_key_equality() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk1 = keypair.public_key();
        let pk2 = PublicKey::from_hex(&pk1.to_hex()).unwrap();
        let pk3 = PublicKey::new(Algorithm::Ed25519, pk1.as_bytes()).unwrap();

        assert_eq!(pk1, pk2);
        assert_eq!(pk1, pk3);
        assert_eq!(pk2, pk3);
    }

    #[test]
    fn test_private_key_equality() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sk1 = keypair.private_key();
        let sk2 = PrivateKey::from_hex(&sk1.to_hex()).unwrap();
        let sk3 = PrivateKey::new(Algorithm::Ed25519, sk1.as_bytes()).unwrap();

        assert_eq!(sk1, sk2);
        assert_eq!(sk1, sk3);
    }

    #[test]
    fn test_signature_equality() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig1 = keypair.sign(b"test");
        let sig2 = Signature::from_hex(&sig1.to_hex()).unwrap();
        let sig3 = Signature::new(Algorithm::Ed25519, sig1.as_bytes()).unwrap();

        assert_eq!(sig1, sig2);
        assert_eq!(sig1, sig3);
    }

    #[test]
    fn test_deterministic_signatures() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let message = b"test message";

        // Ed25519 signatures are deterministic
        let sig1 = keypair.sign(message);
        let sig2 = keypair.sign(message);
        assert_eq!(sig1, sig2);
    }

    #[test]
    fn test_different_messages_different_signatures() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig1 = keypair.sign(b"message1");
        let sig2 = keypair.sign(b"message2");
        assert_ne!(sig1, sig2);
    }

    #[test]
    fn test_algorithm_from_str() {
        assert_eq!(Algorithm::parse("ed25519").unwrap(), Algorithm::Ed25519);
        assert!(Algorithm::parse("unknown").is_err());
    }

    #[test]
    fn test_algorithm_from_u8() {
        assert_eq!(Algorithm::from_u8(0).unwrap(), Algorithm::Ed25519);
        assert!(Algorithm::from_u8(255).is_err());
    }

    #[test]
    fn test_wire_format_roundtrip() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

        // PublicKey
        let pk = keypair.public_key();
        let (algo, bytes) = pk.to_wire();
        let pk2 = PublicKey::from_wire(algo, &bytes).unwrap();
        assert_eq!(pk, pk2);

        // PrivateKey
        let sk = keypair.private_key();
        let (algo, bytes) = sk.to_wire();
        let sk2 = PrivateKey::from_wire(algo, &bytes).unwrap();
        assert_eq!(sk, sk2);

        // Signature
        let sig = keypair.sign(b"test");
        let (algo, bytes) = sig.to_wire();
        let sig2 = Signature::from_wire(algo, &bytes).unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn test_cbor_serialization() {
        use crate::serialization::{canonical_cbor_encode, cbor_decode};

        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

        // PublicKey
        let pk = keypair.public_key();
        let cbor = canonical_cbor_encode(&pk).unwrap();
        let pk2: PublicKey = cbor_decode(&cbor).unwrap();
        assert_eq!(pk, pk2);

        // Signature
        let sig = keypair.sign(b"test");
        let cbor = canonical_cbor_encode(&sig).unwrap();
        let sig2: Signature = cbor_decode(&cbor).unwrap();
        assert_eq!(sig, sig2);
    }

    #[test]
    fn test_algorithm_lengths() {
        assert_eq!(Algorithm::Ed25519.public_key_len(), 32);
        assert_eq!(Algorithm::Ed25519.private_key_len(), 32);
        assert_eq!(Algorithm::Ed25519.signature_len(), 64);
    }

    #[test]
    fn test_algorithm_default() {
        // Algorithm::default() should be Ed25519
        let algo: Algorithm = Algorithm::default();
        assert_eq!(algo, Algorithm::Ed25519);
    }

    #[test]
    fn test_from_hex_odd_length() {
        // Hex strings must have even length
        // Create a valid prefix but odd-length hex part
        let odd_hex = format!("ed25519:{}", "a".repeat(63)); // 63 is odd
        assert!(PublicKey::from_hex(&odd_hex).is_err());
        assert!(PrivateKey::from_hex(&odd_hex).is_err());

        let odd_sig_hex = format!("ed25519:{}", "a".repeat(127)); // 127 is odd
        assert!(Signature::from_hex(&odd_sig_hex).is_err());
    }

    #[test]
    fn test_public_key_debug_format() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk = keypair.public_key();
        let debug = format!("{pk:?}");

        // Debug should show algorithm and abbreviated hex
        assert!(debug.starts_with("PublicKey(ed25519:"));
        assert!(debug.ends_with("...)"));
        // Should be abbreviated (not full 64 chars)
        assert!(debug.len() < 80);
    }

    #[test]
    fn test_public_key_display_format() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk = keypair.public_key();
        let display = format!("{pk}");

        // Display should be same as to_hex
        assert_eq!(display, pk.to_hex());
        assert!(display.starts_with("ed25519:"));
        // Full hex: ed25519: (8 chars) + 64 hex chars = 72
        assert_eq!(display.len(), 72);
    }

    #[test]
    fn test_public_key_as_ref() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let pk = keypair.public_key();
        let bytes: &[u8] = pk.as_ref();
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes, pk.as_bytes());
    }

    #[test]
    fn test_private_key_debug_format() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sk = keypair.private_key();
        let debug = format!("{sk:?}");

        // Debug should hide secret bytes
        assert!(debug.contains("***"));
        assert!(debug.contains("ed25519"));
    }

    #[test]
    fn test_private_key_to_hex() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sk = keypair.private_key();
        let hex = sk.to_hex();

        // to_hex should produce algorithm-prefixed hex
        assert!(hex.starts_with("ed25519:"));
        assert_eq!(hex.len(), 72); // ed25519: (8) + 64 hex chars
    }

    #[test]
    fn test_private_key_as_bytes() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sk = keypair.private_key();
        let bytes = sk.as_bytes();
        assert_eq!(bytes.len(), 32);
    }

    #[test]
    fn test_signature_debug_format() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig = keypair.sign(b"test");
        let debug = format!("{sig:?}");

        // Debug should show algorithm and abbreviated hex
        assert!(debug.starts_with("Signature(ed25519:"));
        assert!(debug.ends_with("...)"));
    }

    #[test]
    fn test_signature_to_hex() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig = keypair.sign(b"test");
        let hex = sig.to_hex();

        // to_hex should produce algorithm-prefixed hex
        assert!(hex.starts_with("ed25519:"));
        // ed25519: (8 chars) + 128 hex chars = 136
        assert_eq!(hex.len(), 136);
    }

    #[test]
    fn test_signature_as_bytes() {
        let keypair = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let sig = keypair.sign(b"test");
        let bytes = sig.as_bytes();
        assert_eq!(bytes.len(), 64);
    }

    #[test]
    fn test_algorithm_display() {
        let algo = Algorithm::Ed25519;
        let display = format!("{algo}");
        assert_eq!(display, "ed25519");
    }
}
