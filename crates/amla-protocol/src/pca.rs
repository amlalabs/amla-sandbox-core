//! PCA (PIC Causal Authority) implementation.
//!
//! PCA is the core authorization structure that represents the authority
//! available to a designated executor at one point in a causal chain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::capability::CapabilityData;
use crate::error::{Error, Result};
use crate::executor::DesignatedExecutor;
use crate::hash::PcaHash;
use crate::identity::{KeyPair, PublicKey, Signature};
use crate::serialization::{canonical_cbor_encode, cbor_decode};
use crate::version::Version;

/// Wire format for PCA signable content.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PcaSignable {
    version: (u32, u32),
    capabilities: Vec<CapabilityData>,
    designated_executor: DesignatedExecutor,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    prev_hash: Option<String>,
    /// Root transaction hash (stable across the chain).
    ///
    /// For the root PCA, this is intentionally omitted to avoid circular hashing.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    root_hash: Option<String>,
    expires_at: String,
    issuer: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    payload: Option<Vec<u8>>,
}

/// Wire format for PCA signable content (borrowed, for serialization only).
#[derive(Debug, Serialize)]
struct PcaSignableRef<'a> {
    version: (u32, u32),
    capabilities: &'a [CapabilityData],
    designated_executor: &'a DesignatedExecutor,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    prev_hash: Option<String>,
    /// Root transaction hash (stable across the chain).
    ///
    /// Root PCAs omit this so their hash can serve as the transaction id.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    root_hash: Option<String>,
    expires_at: String,
    issuer: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    payload: Option<&'a [u8]>,
}

/// Wire format for full PCA (includes signature).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PcaWire {
    #[serde(flatten)]
    signable: PcaSignable,
    signature: String,
}

/// Wire format for full PCA (borrowed, for serialization only).
#[derive(Debug, Serialize)]
struct PcaWireRef<'a> {
    #[serde(flatten)]
    signable: PcaSignableRef<'a>,
    signature: String,
}

/// PIC Causal Authority - the core authorization structure.
///
/// Represents the authority available to a designated executor
/// at one point in a causal chain.
///
/// Immutable once created. ID = hash of signable content.
///
/// # Security Properties (enforced by gateway, not protocol)
///
/// 1. **Designated executor binding** - only this key can invoke
/// 2. **Chain integrity** - `prev_hash` links to parent PCA
/// 3. **Signature validity** - issuer signed this PCA
/// 4. **Temporal validity** - not expired
///
/// # Example
///
/// ```
/// use amla_protocol::{Pca, PcaBuilder, KeyPair, Algorithm, CapabilityData, Version};
/// use chrono::{Utc, Duration};
/// use serde_json::json;
///
/// let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
/// let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
///
/// let cap = CapabilityData::from_json(
///     "cap:refund",
///     "function",
///     &json!({"name": "payments.refund", "max_amount": 500}),
/// ).unwrap();
///
/// let pca = PcaBuilder::new()
///     .version(Version::new(0, 1))
///     .add_capability(cap)
///     .designated_executor(agent.public_key())
///     .expires_at(Utc::now() + Duration::hours(1))
///     .build_and_sign(&gateway)
///     .unwrap();
///
/// assert!(pca.try_verify_signature().is_ok());
/// ```
#[derive(Debug, Clone)]
pub struct Pca {
    /// Protocol version.
    version: Version,

    /// Capabilities granted by this PCA.
    capabilities: Vec<CapabilityData>,

    /// Who can continue this transaction at the next hop.
    designated_executor: DesignatedExecutor,

    /// Hash of the parent PCA (None for root).
    prev_hash: Option<PcaHash>,

    /// Root transaction hash (stable across the chain).
    ///
    /// Root PCAs omit this so their hash can serve as the transaction id.
    root_hash: Option<PcaHash>,

    /// Expiration time (must be timezone-aware).
    expires_at: DateTime<Utc>,

    /// Public key of the PCA issuer.
    issuer: PublicKey,

    /// Ed25519 signature over signable content.
    signature: Signature,

    /// Optional arbitrary payload (CBOR bytes).
    payload: Option<Vec<u8>>,
}

impl Serialize for Pca {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let wire = PcaWireRef {
            signable: self.build_signable_ref(),
            signature: self.signature.to_hex(),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Pca {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = PcaWire::deserialize(deserializer)?;
        Self::from_wire(wire).map_err(serde::de::Error::custom)
    }
}

impl Pca {
    /// Get the protocol version.
    #[must_use]
    pub fn version(&self) -> Version {
        self.version
    }

    /// Get the capabilities.
    #[must_use]
    pub fn capabilities(&self) -> &[CapabilityData] {
        &self.capabilities
    }

    /// Get a capability by key.
    #[must_use]
    pub fn capability(&self, key: &str) -> Option<&CapabilityData> {
        self.capabilities.iter().find(|c| c.key() == key)
    }

    /// Get the designated executor.
    #[must_use]
    pub fn designated_executor(&self) -> &DesignatedExecutor {
        &self.designated_executor
    }

    /// Get the designated executor's public key if it's a direct key designation.
    ///
    /// Returns `None` if the designated executor is a characteristic or CTA reference.
    #[must_use]
    pub fn designated_executor_key(&self) -> Option<&PublicKey> {
        self.designated_executor.as_public_key()
    }

    /// Get the hash of the parent PCA.
    #[must_use]
    pub fn prev_hash(&self) -> Option<&PcaHash> {
        self.prev_hash.as_ref()
    }

    /// Get the root transaction hash.
    #[must_use]
    pub fn root_hash(&self) -> Option<&PcaHash> {
        self.root_hash.as_ref()
    }

    /// Get the expiration time.
    #[must_use]
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    /// Get the issuer's public key.
    #[must_use]
    pub fn issuer(&self) -> &PublicKey {
        &self.issuer
    }

    /// Get the signature.
    #[must_use]
    pub fn signature(&self) -> &Signature {
        &self.signature
    }

    /// Get the optional payload as raw CBOR bytes.
    #[must_use]
    pub fn payload_bytes(&self) -> Option<&[u8]> {
        self.payload.as_deref()
    }

    /// Decode the optional payload into a typed struct.
    ///
    /// # Errors
    ///
    /// Returns an error if payload is None or deserialization fails.
    pub fn payload<T: serde::de::DeserializeOwned>(&self) -> Result<Option<T>> {
        match &self.payload {
            Some(bytes) => Ok(Some(cbor_decode(bytes)?)),
            None => Ok(None),
        }
    }

    /// Check if this is a root PCA (no parent).
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.prev_hash.is_none()
    }

    /// Create an empty test PCA for testing purposes.
    ///
    /// This creates a minimal PCA with no capabilities and a dummy signature.
    /// It should only be used for testing runtime creation without validation.
    #[must_use]
    pub fn empty_test() -> Self {
        use chrono::Duration;

        Self {
            version: Version::default(),
            capabilities: Vec::new(),
            designated_executor: DesignatedExecutor::from_public_key(PublicKey::zero()),
            prev_hash: None,
            root_hash: None,
            expires_at: Utc::now() + Duration::days(365),
            issuer: PublicKey::zero(),
            signature: Signature::zero(),
            payload: None,
        }
    }

    /// Compute the unique identifier (hash of signable content) without panicking.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn try_hash(&self) -> Result<PcaHash> {
        let signable = self.try_to_signable()?;
        Ok(PcaHash::compute(&signable))
    }

    /// Check if the PCA has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    /// Check if the PCA is expired at a specific time.
    #[must_use]
    pub fn is_expired_at(&self, time: DateTime<Utc>) -> bool {
        time >= self.expires_at
    }

    /// Verify the PCA's signature without panicking.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails or the signature is invalid.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn try_verify_signature(&self) -> Result<()> {
        let signable = self.try_to_signable()?;
        self.issuer.verify(&signable, &self.signature)
    }

    /// Build the signable data structure.
    fn build_signable_ref(&self) -> PcaSignableRef<'_> {
        PcaSignableRef {
            version: self.version.to_tuple(),
            capabilities: &self.capabilities,
            designated_executor: &self.designated_executor,
            prev_hash: self.prev_hash.as_ref().map(PcaHash::to_hex),
            root_hash: self.root_hash.as_ref().map(PcaHash::to_hex),
            expires_at: self.expires_at.to_rfc3339(),
            issuer: self.issuer.to_hex(),
            payload: self.payload.as_deref(),
        }
    }

    /// Get canonical bytes for signing/hashing without panicking.
    ///
    /// Excludes signature field.
    /// Deterministic: same PCA -> same bytes always.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn try_to_signable(&self) -> Result<Vec<u8>> {
        let signable = self.build_signable_ref();
        canonical_cbor_encode(&signable)
    }

    /// Serialize to CBOR wire format (includes signature).
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn to_cbor(&self) -> Result<Vec<u8>> {
        let wire = PcaWireRef {
            signable: self.build_signable_ref(),
            signature: self.signature.to_hex(),
        };
        canonical_cbor_encode(&wire)
    }

    /// Deserialize from CBOR.
    ///
    /// # Errors
    ///
    /// Returns an error if deserialization fails.
    #[must_use = "this returns a Result that may contain an error"]
    pub fn from_cbor(data: &[u8]) -> Result<Self> {
        let wire: PcaWire = cbor_decode(data)?;
        Self::from_wire(wire)
    }

    /// Create from wire format.
    fn from_wire(wire: PcaWire) -> Result<Self> {
        let version = Version::from_tuple(wire.signable.version);

        if let Some(dup_key) = crate::find_duplicate_capability_key(&wire.signable.capabilities) {
            return Err(Error::DuplicateCapabilityKey(dup_key.to_string()));
        }

        let prev_hash = wire
            .signable
            .prev_hash
            .map(|h| PcaHash::from_hex(&h))
            .transpose()?;

        let root_hash = wire
            .signable
            .root_hash
            .map(|h| PcaHash::from_hex(&h))
            .transpose()?;

        let expires_at = DateTime::parse_from_rfc3339(&wire.signable.expires_at)
            .map_err(|e| Error::InvalidPca(format!("invalid expires_at: {e}")))?
            .with_timezone(&Utc);

        let issuer = PublicKey::from_hex(&wire.signable.issuer)?;
        let signature = Signature::from_hex(&wire.signature)?;

        Ok(Self {
            version,
            capabilities: wire.signable.capabilities,
            designated_executor: wire.signable.designated_executor,
            prev_hash,
            root_hash,
            expires_at,
            issuer,
            signature,
            payload: wire.signable.payload,
        })
    }
}

/// Builder for creating PCAs.
///
/// # Example
///
/// ```
/// use amla_protocol::{PcaBuilder, KeyPair, Algorithm, CapabilityData, Version};
/// use chrono::{Utc, Duration};
/// use serde_json::json;
///
/// let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
/// let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
///
/// let cap = CapabilityData::from_json("cap:call", "function", &json!({"name": "api.call"})).unwrap();
///
/// let pca = PcaBuilder::new()
///     .version(Version::new(0, 1))
///     .add_capability(cap)
///     .designated_executor(agent.public_key())
///     .expires_at(Utc::now() + Duration::hours(1))
///     .build_and_sign(&gateway)
///     .unwrap();
/// ```
#[derive(Default)]
pub struct PcaBuilder {
    version: Option<Version>,
    capabilities: Vec<CapabilityData>,
    designated_executor: Option<DesignatedExecutor>,
    prev_hash: Option<PcaHash>,
    root_hash: Option<PcaHash>,
    expires_at: Option<DateTime<Utc>>,
    payload: Option<Vec<u8>>,
}

impl PcaBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the protocol version.
    #[must_use]
    pub fn version(mut self, version: Version) -> Self {
        self.version = Some(version);
        self
    }

    /// Add a capability.
    #[must_use]
    pub fn add_capability(mut self, cap: CapabilityData) -> Self {
        self.capabilities.push(cap);
        self
    }

    /// Add multiple capabilities.
    #[must_use]
    pub fn capabilities(mut self, caps: Vec<CapabilityData>) -> Self {
        self.capabilities = caps;
        self
    }

    /// Set the designated executor.
    ///
    /// Accepts either a `PublicKey` for direct key designation, or a
    /// `DesignatedExecutor` for more complex designation types.
    ///
    /// # Example
    ///
    /// ```
    /// use amla_protocol::{PcaBuilder, KeyPair, Algorithm, DesignatedExecutor};
    ///
    /// let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
    ///
    /// // Direct public key (most common)
    /// let builder = PcaBuilder::new().designated_executor(agent.public_key());
    ///
    /// // Or explicit DesignatedExecutor
    /// let executor = DesignatedExecutor::from_public_key(agent.public_key());
    /// let builder = PcaBuilder::new().designated_executor(executor);
    /// ```
    #[must_use]
    pub fn designated_executor(mut self, executor: impl Into<DesignatedExecutor>) -> Self {
        self.designated_executor = Some(executor.into());
        self
    }

    /// Set the parent PCA hash.
    #[must_use]
    pub fn prev_hash(mut self, hash: PcaHash) -> Self {
        self.prev_hash = Some(hash);
        self
    }

    /// Set the root transaction hash.
    ///
    /// Required for non-root PCAs so the transaction id is stable across hops.
    #[must_use]
    pub fn root_hash(mut self, hash: PcaHash) -> Self {
        self.root_hash = Some(hash);
        self
    }

    /// Set the parent PCA (prev hash + root hash) in one step.
    ///
    /// This ensures child PCAs remain bound to the root transaction hash.
    ///
    /// # Errors
    ///
    /// Returns an error if hashing the parent PCA fails.
    pub fn parent_pca(mut self, parent: &Pca) -> Result<Self> {
        let parent_hash = parent.try_hash()?;
        self.prev_hash = Some(parent_hash);
        let root_hash = match parent.root_hash() {
            Some(root) => *root,
            None => {
                if parent.is_root() {
                    parent_hash
                } else {
                    return Err(Error::InvalidPca(
                        "non-root parent is missing root_hash".to_string(),
                    ));
                }
            }
        };
        self.root_hash = Some(root_hash);
        Ok(self)
    }

    /// Set the expiration time.
    #[must_use]
    pub fn expires_at(mut self, time: DateTime<Utc>) -> Self {
        self.expires_at = Some(time);
        self
    }

    /// Set optional payload from a serializable value.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn payload<T: serde::Serialize>(mut self, payload: &T) -> Result<Self> {
        self.payload = Some(canonical_cbor_encode(payload)?);
        Ok(self)
    }

    /// Set optional payload from raw CBOR bytes.
    #[must_use]
    pub fn payload_bytes(mut self, bytes: Vec<u8>) -> Self {
        self.payload = Some(bytes);
        self
    }

    /// Build and sign the PCA.
    ///
    /// # Errors
    ///
    /// Returns an error if required fields are missing or if there are
    /// duplicate capability keys.
    pub fn build_and_sign(self, issuer: &KeyPair) -> Result<Pca> {
        let version = self
            .version
            .ok_or_else(|| Error::MissingField("version".to_string()))?;

        if self.capabilities.is_empty() {
            return Err(Error::InvalidPca(
                "at least one capability required".to_string(),
            ));
        }

        // Check for duplicate capability keys
        if let Some(dup_key) = crate::find_duplicate_capability_key(&self.capabilities) {
            return Err(Error::DuplicateCapabilityKey(dup_key.to_string()));
        }

        let designated_executor = self
            .designated_executor
            .ok_or_else(|| Error::MissingField("designated_executor".to_string()))?;

        let expires_at = self
            .expires_at
            .ok_or_else(|| Error::MissingField("expires_at".to_string()))?;

        // Root hash is required for non-root PCAs.
        // Root PCAs intentionally omit it so their PCA hash serves as the tx id.
        let root_hash = if let Some(hash) = self.root_hash {
            if self.prev_hash.is_none() {
                return Err(Error::InvalidPca(
                    "root PCA must omit root_hash".to_string(),
                ));
            }
            Some(hash)
        } else {
            if self.prev_hash.is_some() {
                return Err(Error::MissingField("root_hash".to_string()));
            }
            None
        };

        // Build signable structure
        let signable = PcaSignableRef {
            version: version.to_tuple(),
            capabilities: &self.capabilities,
            designated_executor: &designated_executor,
            prev_hash: self.prev_hash.as_ref().map(PcaHash::to_hex),
            root_hash: root_hash.as_ref().map(PcaHash::to_hex),
            expires_at: expires_at.to_rfc3339(),
            issuer: issuer.public_key().to_hex(),
            payload: self.payload.as_deref(),
        };

        let signable_bytes = canonical_cbor_encode(&signable)?;

        // Sign
        let signature = issuer.sign(&signable_bytes);

        Ok(Pca {
            version,
            capabilities: self.capabilities,
            designated_executor,
            prev_hash: self.prev_hash,
            root_hash,
            expires_at,
            issuer: issuer.public_key(),
            signature,
            payload: self.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Algorithm;
    use chrono::Duration;

    fn sample_capability(key: &str) -> CapabilityData {
        CapabilityData::from_json(
            key,
            "function",
            &serde_json::json!({"name": "test.function"}),
        )
        .unwrap()
    }

    #[test]
    fn test_create_root_pca() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        assert!(pca.is_root());
        assert!(!pca.is_expired());
        assert!(pca.try_verify_signature().is_ok());
        assert_eq!(pca.issuer(), &gateway.public_key());
        assert_eq!(
            pca.designated_executor().as_public_key(),
            Some(&agent.public_key())
        );
    }

    #[test]
    fn test_create_child_pca() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);

        let root = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent1.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        let child = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(Utc::now() + Duration::minutes(30))
            .build_and_sign(&agent1)
            .unwrap();

        assert!(!child.is_root());
        assert_eq!(child.prev_hash(), Some(&root.try_hash().unwrap()));
        assert!(child.try_verify_signature().is_ok());
        assert_eq!(child.issuer(), &agent1.public_key());
    }

    #[test]
    fn test_capability_by_key() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:read"))
            .add_capability(sample_capability("cap:write"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        assert!(pca.capability("cap:read").is_some());
        assert!(pca.capability("cap:write").is_some());
        assert!(pca.capability("cap:delete").is_none());
    }

    #[test]
    fn test_expired_pca() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() - Duration::hours(1)) // Already expired
            .build_and_sign(&gateway)
            .unwrap();

        assert!(pca.is_expired());
    }

    #[test]
    fn test_cbor_roundtrip() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .payload(&serde_json::json!({"key": "value"}))
            .unwrap()
            .build_and_sign(&gateway)
            .unwrap();

        let cbor = pca.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor).unwrap();

        assert_eq!(pca.try_hash().unwrap(), pca2.try_hash().unwrap());
        assert!(pca2.try_verify_signature().is_ok());
    }

    #[test]
    fn test_parent_pca_rejects_missing_root_hash_on_non_root() {
        let issuer = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let cap = sample_capability("cap:test");
        let prev_hash = PcaHash::compute(b"parent");

        // Construct a non-root PCA that incorrectly omits root_hash.
        let signable = PcaSignable {
            version: Version::new(0, 1).to_tuple(),
            capabilities: vec![cap.clone()],
            designated_executor: DesignatedExecutor::from_public_key(agent.public_key()),
            prev_hash: Some(prev_hash.to_hex()),
            root_hash: None,
            expires_at: expires.to_rfc3339(),
            issuer: issuer.public_key().to_hex(),
            payload: None,
        };

        let signable_bytes = canonical_cbor_encode(&signable).unwrap();
        let signature = issuer.sign(&signable_bytes).to_hex();
        let wire = PcaWire {
            signable,
            signature,
        };
        let bad_parent = Pca::from_wire(wire).unwrap();

        let result = PcaBuilder::new().parent_pca(&bad_parent);
        assert!(matches!(result, Err(Error::InvalidPca(_))));
    }

    #[test]
    fn test_deterministic_hash() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca1 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Same content, same issuer -> same hash
        assert_eq!(pca1.try_hash().unwrap(), pca2.try_hash().unwrap());
    }

    #[test]
    fn test_missing_fields() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);

        // Missing version
        assert!(
            PcaBuilder::new()
                .add_capability(sample_capability("cap:test"))
                .designated_executor(gateway.public_key())
                .expires_at(Utc::now() + Duration::hours(1))
                .build_and_sign(&gateway)
                .is_err()
        );

        // Missing capabilities
        assert!(
            PcaBuilder::new()
                .version(Version::new(0, 1))
                .designated_executor(gateway.public_key())
                .expires_at(Utc::now() + Duration::hours(1))
                .build_and_sign(&gateway)
                .is_err()
        );

        // Missing designated_executor
        assert!(
            PcaBuilder::new()
                .version(Version::new(0, 1))
                .add_capability(sample_capability("cap:test"))
                .expires_at(Utc::now() + Duration::hours(1))
                .build_and_sign(&gateway)
                .is_err()
        );

        // Missing expires_at
        assert!(
            PcaBuilder::new()
                .version(Version::new(0, 1))
                .add_capability(sample_capability("cap:test"))
                .designated_executor(gateway.public_key())
                .build_and_sign(&gateway)
                .is_err()
        );
    }

    #[test]
    fn test_duplicate_capability_keys_rejected() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        // Try to add two capabilities with the same key
        let result = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:same"))
            .add_capability(sample_capability("cap:same")) // Duplicate!
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway);

        assert!(result.is_err());
        match result.unwrap_err() {
            Error::DuplicateCapabilityKey(key) => {
                assert_eq!(key, "cap:same");
            }
            other => panic!("Expected DuplicateCapabilityKey error, got: {other}"),
        }
    }

    #[test]
    fn test_signature_tampering() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let attacker = KeyPair::from_seed(Algorithm::Ed25519, &[10; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        // Get CBOR and decode to wire format
        let cbor = pca.to_cbor().unwrap();
        let mut wire: PcaWire = cbor_decode(&cbor).unwrap();

        // Tamper with designated executor
        wire.signable.designated_executor =
            DesignatedExecutor::from_public_key(attacker.public_key());

        // Re-encode
        let tampered_cbor = canonical_cbor_encode(&wire).unwrap();
        let tampered_pca = Pca::from_cbor(&tampered_cbor).unwrap();

        // Signature should NOT verify
        assert!(tampered_pca.try_verify_signature().is_err());
    }

    #[test]
    fn test_multiple_capabilities() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let cap1 = CapabilityData::from_json(
            "cap:read",
            "function",
            &serde_json::json!({"name": "api.read"}),
        )
        .unwrap();
        let cap2 = CapabilityData::from_json(
            "cap:write",
            "function",
            &serde_json::json!({"name": "api.write"}),
        )
        .unwrap();
        let cap3 = CapabilityData::from_json(
            "cap:data",
            "resource",
            &serde_json::json!({"path": "/data/*"}),
        )
        .unwrap();

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap1)
            .add_capability(cap2)
            .add_capability(cap3)
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        assert_eq!(pca.capabilities().len(), 3);
        assert!(pca.try_verify_signature().is_ok());

        // Roundtrip preserves all capabilities
        let cbor = pca.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor).unwrap();
        assert_eq!(pca2.capabilities().len(), 3);
        assert!(pca2.try_verify_signature().is_ok());

        // Can look up by key
        assert!(pca2.capability("cap:read").is_some());
        assert!(pca2.capability("cap:write").is_some());
        assert!(pca2.capability("cap:data").is_some());
    }

    #[test]
    fn test_typed_payload() {
        use serde::{Deserialize, Serialize};

        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct RequestContext {
            request_id: String,
            priority: i32,
        }

        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let context = RequestContext {
            request_id: "abc-123".to_string(),
            priority: 1,
        };

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .payload(&context)
            .unwrap()
            .build_and_sign(&gateway)
            .unwrap();

        // Decode payload
        let decoded: Option<RequestContext> = pca.payload().unwrap();
        assert_eq!(decoded, Some(context));

        // Roundtrip
        let cbor = pca.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor).unwrap();
        let decoded2: Option<RequestContext> = pca2.payload().unwrap();
        assert_eq!(decoded2.unwrap().request_id, "abc-123");
    }

    #[test]
    fn test_no_payload() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        assert!(pca.payload_bytes().is_none());
        let decoded: Option<serde_json::Value> = pca.payload().unwrap();
        assert!(decoded.is_none());
    }

    #[test]
    fn test_version_preserved() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(1, 5))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        assert_eq!(pca.version(), Version::new(1, 5));

        let cbor = pca.to_cbor().unwrap();
        let pca2 = Pca::from_cbor(&cbor).unwrap();
        assert_eq!(pca2.version(), Version::new(1, 5));
    }

    #[test]
    fn test_is_expired_at_boundary() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Just before expiry - not expired
        assert!(!pca.is_expired_at(expires - Duration::seconds(1)));
        // Exactly at expiry - expired (time >= expires_at)
        assert!(pca.is_expired_at(expires));
        // Just after expiry - expired
        assert!(pca.is_expired_at(expires + Duration::seconds(1)));
    }

    #[test]
    fn test_chain_of_three() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let agent3 = KeyPair::from_seed(Algorithm::Ed25519, &[4; 32]);
        let expires = Utc::now() + Duration::hours(1);

        // Root: gateway -> agent1
        let root = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        // Middle: agent1 -> agent2
        let middle = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent2.public_key())
            .parent_pca(&root)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent1)
            .unwrap();

        // Leaf: agent2 -> agent3
        let leaf = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent3.public_key())
            .parent_pca(&middle)
            .unwrap()
            .expires_at(expires)
            .build_and_sign(&agent2)
            .unwrap();

        // Verify chain structure
        assert!(root.is_root());
        assert!(!middle.is_root());
        assert!(!leaf.is_root());

        assert_eq!(middle.prev_hash(), Some(&root.try_hash().unwrap()));
        assert_eq!(leaf.prev_hash(), Some(&middle.try_hash().unwrap()));

        // All signatures valid
        assert!(root.try_verify_signature().is_ok());
        assert!(middle.try_verify_signature().is_ok());
        assert!(leaf.try_verify_signature().is_ok());

        // Issuers are correct
        assert_eq!(root.issuer(), &gateway.public_key());
        assert_eq!(middle.issuer(), &agent1.public_key());
        assert_eq!(leaf.issuer(), &agent2.public_key());
    }

    #[test]
    fn test_cbor_invalid_data() {
        // Empty data
        assert!(Pca::from_cbor(&[]).is_err());

        // Random garbage
        assert!(Pca::from_cbor(&[0xFF, 0xFE, 0x00, 0x01]).is_err());

        // Valid CBOR but wrong structure
        let wrong = canonical_cbor_encode(&serde_json::json!({"wrong": "structure"})).unwrap();
        assert!(Pca::from_cbor(&wrong).is_err());
    }

    #[test]
    fn test_hash_uniqueness() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent1 = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let agent2 = KeyPair::from_seed(Algorithm::Ed25519, &[3; 32]);
        let expires = Utc::now() + Duration::hours(1);

        // Same everything except designated_executor
        let pca1 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent1.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent2.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        assert_ne!(pca1.try_hash().unwrap(), pca2.try_hash().unwrap());
    }

    #[test]
    fn test_hash_includes_capabilities() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let cap1 =
            CapabilityData::from_json("cap:test", "test", &serde_json::json!({"x": 1})).unwrap();
        let cap2 =
            CapabilityData::from_json("cap:test", "test", &serde_json::json!({"x": 2})).unwrap();

        let pca1 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap1)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(cap2)
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway)
            .unwrap();

        assert_ne!(pca1.try_hash().unwrap(), pca2.try_hash().unwrap());
    }

    #[test]
    fn test_different_issuers_different_signatures() {
        let gateway1 = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let gateway2 = KeyPair::from_seed(Algorithm::Ed25519, &[5; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);
        let expires = Utc::now() + Duration::hours(1);

        let pca1 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway1)
            .unwrap();

        let pca2 = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:test"))
            .designated_executor(agent.public_key())
            .expires_at(expires)
            .build_and_sign(&gateway2)
            .unwrap();

        // Different issuers -> different hashes (issuer is part of hash)
        assert_ne!(pca1.try_hash().unwrap(), pca2.try_hash().unwrap());
        assert_ne!(pca1.issuer(), pca2.issuer());
        assert!(pca1.try_verify_signature().is_ok());
        assert!(pca2.try_verify_signature().is_ok());
    }

    #[test]
    fn test_from_cbor_rejects_duplicate_keys() {
        let gateway = KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]);
        let agent = KeyPair::from_seed(Algorithm::Ed25519, &[2; 32]);

        let pca = PcaBuilder::new()
            .version(Version::new(0, 1))
            .add_capability(sample_capability("cap:a"))
            .designated_executor(agent.public_key())
            .expires_at(Utc::now() + Duration::hours(1))
            .build_and_sign(&gateway)
            .unwrap();

        let cbor = pca.to_cbor().unwrap();
        let mut wire: PcaWire = cbor_decode(&cbor).unwrap();

        // Inject duplicate capability key
        wire.signable
            .capabilities
            .push(wire.signable.capabilities[0].clone());

        let tampered_cbor = canonical_cbor_encode(&wire).unwrap();
        let err = Pca::from_cbor(&tampered_cbor).unwrap_err();

        match err {
            Error::DuplicateCapabilityKey(key) => assert_eq!(key, "cap:a"),
            other => panic!("Expected DuplicateCapabilityKey error, got: {other}"),
        }
    }
}
