//! SHA-256 hash type for PCA identification.
//!
//! # Why No Algorithm Abstraction?
//!
//! Unlike [`crate::identity`] types which support multiple algorithms, `PcaHash`
//! is fixed to SHA-256. This is intentional:
//!
//! - **Protocol-level decision**: The hash algorithm is part of the protocol spec,
//!   not a per-message choice. Changing it requires a protocol version bump.
//! - **Simpler wire format**: No algorithm discriminator needed in serialization.
//! - **Consistent chain linking**: All PCAs in a chain use the same hash algorithm.
//! - **Security uniformity**: One well-audited algorithm rather than multiple.
//!
//! If a hash algorithm migration is ever needed, it would be handled via protocol
//! versioning rather than runtime algorithm selection.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// SHA-256 hash of a PCA.
///
/// Also serves as the PCA's unique identifier.
/// Immutable, 32 bytes exactly.
///
/// # Example
///
/// ```
/// use amla_protocol::PcaHash;
///
/// let hash = PcaHash::compute(b"some data");
/// println!("{}", hash.to_hex()); // 64 character hex string
///
/// let hash2 = PcaHash::from_hex(&hash.to_hex()).unwrap();
/// assert_eq!(hash, hash2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "serde_bytes::ByteBuf", into = "serde_bytes::ByteBuf")]
pub struct PcaHash([u8; 32]);

impl PcaHash {
    /// Hash length in bytes.
    pub const LENGTH: usize = 32;

    /// Create a new `PcaHash` from raw bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the slice is not exactly 32 bytes.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn new(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::LENGTH {
            return Err(Error::InvalidHashLength(bytes.len()));
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(bytes);
        Ok(Self(arr))
    }

    /// Create from a fixed-size array (infallible).
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Compute SHA-256 hash of data.
    #[must_use]
    pub fn compute(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        Self(result.into())
    }

    /// Get the raw bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Convert to hex string (64 characters).
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Create from hex string.
    ///
    /// # Errors
    ///
    /// Returns an error if the hex string is invalid or not 64 characters.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn from_hex(s: &str) -> Result<Self> {
        if s.len() != 64 {
            return Err(Error::InvalidHashLength(s.len() / 2));
        }
        let mut arr = [0u8; 32];
        hex::decode_to_slice(s, &mut arr)?;
        Ok(Self(arr))
    }
}

impl AsRef<[u8]> for PcaHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<[u8; 32]> for PcaHash {
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl TryFrom<&[u8]> for PcaHash {
    type Error = Error;

    fn try_from(bytes: &[u8]) -> Result<Self> {
        Self::new(bytes)
    }
}

impl TryFrom<serde_bytes::ByteBuf> for PcaHash {
    type Error = Error;

    fn try_from(buf: serde_bytes::ByteBuf) -> Result<Self> {
        Self::new(&buf)
    }
}

impl From<PcaHash> for serde_bytes::ByteBuf {
    fn from(hash: PcaHash) -> Self {
        serde_bytes::ByteBuf::from(hash.0.to_vec())
    }
}

impl std::fmt::Display for PcaHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let hash = PcaHash::compute(b"hello world");
        assert_eq!(hash.as_bytes().len(), 32);

        // SHA-256 of "hello world" is well-known
        assert_eq!(
            hash.to_hex(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_hex_roundtrip() {
        let hash = PcaHash::compute(b"test data");
        let hex = hash.to_hex();
        let hash2 = PcaHash::from_hex(&hex).unwrap();
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_invalid_length() {
        assert!(PcaHash::new(&[0u8; 31]).is_err());
        assert!(PcaHash::new(&[0u8; 33]).is_err());
        assert!(PcaHash::from_hex("abcd").is_err());
    }

    #[test]
    fn test_deterministic() {
        let h1 = PcaHash::compute(b"same input");
        let h2 = PcaHash::compute(b"same input");
        assert_eq!(h1, h2);

        let h3 = PcaHash::compute(b"different input");
        assert_ne!(h1, h3);
    }

    #[test]
    fn test_new_valid_length() {
        // Test the success path of new() with exactly 32 bytes
        let bytes = [0x42u8; 32];
        let hash = PcaHash::new(&bytes).unwrap();
        assert_eq!(hash.as_bytes(), &bytes);
    }

    #[test]
    fn test_from_bytes_infallible() {
        // Test the const fn from_bytes constructor
        let bytes: [u8; 32] = [0xAB; 32];
        let hash = PcaHash::from_bytes(bytes);
        assert_eq!(hash.as_bytes(), &bytes);
    }

    #[test]
    fn test_as_ref_slice() {
        // Test AsRef<[u8]> impl
        let hash = PcaHash::compute(b"test");
        let slice: &[u8] = hash.as_ref();
        assert_eq!(slice.len(), 32);
        assert_eq!(slice, hash.as_bytes());
    }

    #[test]
    fn test_from_fixed_array() {
        // Test From<[u8; 32]> impl
        let bytes: [u8; 32] = [0x12; 32];
        let hash: PcaHash = bytes.into();
        assert_eq!(hash.as_bytes(), &bytes);
    }

    #[test]
    fn test_try_from_slice() {
        // Test TryFrom<&[u8]> impl
        let bytes = [0x34u8; 32];
        let hash: PcaHash = (&bytes[..]).try_into().unwrap();
        assert_eq!(hash.as_bytes(), &bytes);

        // Wrong length should fail
        let short: Result<PcaHash> = (&[0u8; 16][..]).try_into();
        assert!(short.is_err());
    }

    #[test]
    fn test_try_from_bytebuf() {
        // Test TryFrom<serde_bytes::ByteBuf> impl
        let bytes = [0x56u8; 32];
        let buf = serde_bytes::ByteBuf::from(bytes.to_vec());
        let hash: PcaHash = buf.try_into().unwrap();
        assert_eq!(hash.as_bytes(), &bytes);
    }

    #[test]
    fn test_into_bytebuf() {
        // Test From<PcaHash> for serde_bytes::ByteBuf
        let hash = PcaHash::compute(b"test");
        let buf: serde_bytes::ByteBuf = hash.into();
        assert_eq!(buf.len(), 32);
    }

    #[test]
    fn test_display_format() {
        // Test Display impl produces same result as to_hex
        let hash = PcaHash::compute(b"hello world");
        let display = format!("{hash}");
        assert_eq!(display, hash.to_hex());
        assert_eq!(display.len(), 64);
    }
}
