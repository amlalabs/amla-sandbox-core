//! WebAssembly bindings for browser usage.
//!
//! This module provides JavaScript-friendly wrappers around the core protocol types.
//! Enable with the `wasm` feature.
//!
//! # Example (JavaScript)
//!
//! ```javascript
//! import init, { JsIdentity, JsPcaBuilder, JsCapabilityData } from 'amla-protocol';
//!
//! await init();
//!
//! // Create identities
//! const gateway = JsIdentity.generate();
//! const agent = JsIdentity.generate();
//!
//! // Create a capability (key, type, data)
//! const cap = new JsCapabilityData("cap:claims", "function", {
//!     name: "insurance.process_claim",
//!     max_amount: 2500000
//! });
//!
//! // Build and sign a PCA
//! const pca = new JsPcaBuilder()
//!     .addCapability(cap)
//!     .designatedExecutor(agent.publicKeyHex())
//!     .expiresInHours(1)
//!     .buildAndSign(gateway);
//!
//! console.log("PCA hash:", pca.hashHex());
//! console.log("Valid signature:", pca.verifySignature());
//! ```

use wasm_bindgen::prelude::*;

use crate::{
    Algorithm, CapabilityData, KeyPair, PROTOCOL_VERSION, Pca, PcaBuilder, PcaHash, PublicKey,
    Version,
};
use chrono::{Duration, Utc};

/// JavaScript-friendly Identity/KeyPair wrapper.
///
/// Wraps an Ed25519 keypair for use in browser contexts.
#[wasm_bindgen]
pub struct JsIdentity {
    inner: KeyPair,
}

#[wasm_bindgen]
impl JsIdentity {
    /// Generate a new random identity (keypair).
    #[wasm_bindgen]
    pub fn generate() -> JsIdentity {
        JsIdentity {
            inner: KeyPair::from_seed(Algorithm::Ed25519, &[1; 32]),
        }
    }

    /// Get the public key as a hex string.
    #[wasm_bindgen(js_name = publicKeyHex)]
    pub fn public_key_hex(&self) -> String {
        self.inner.public_key().to_hex()
    }

    /// Get the public key as bytes.
    #[wasm_bindgen(js_name = publicKeyBytes)]
    pub fn public_key_bytes(&self) -> Vec<u8> {
        self.inner.public_key().as_bytes().to_vec()
    }

    /// Sign a message and return the signature as hex.
    #[wasm_bindgen(js_name = signHex)]
    pub fn sign_hex(&self, message: &[u8]) -> String {
        self.inner.sign(message).to_hex()
    }

    /// Sign a message and return the signature as bytes.
    #[wasm_bindgen(js_name = signBytes)]
    pub fn sign_bytes(&self, message: &[u8]) -> Vec<u8> {
        self.inner.sign(message).as_bytes().to_vec()
    }

    /// Verify a signature (hex) against a message.
    #[wasm_bindgen(js_name = verifyHex)]
    pub fn verify_hex(&self, message: &[u8], signature_hex: &str) -> bool {
        match crate::Signature::from_hex(signature_hex) {
            Ok(sig) => self.inner.verify(message, &sig).is_ok(),
            Err(_) => false,
        }
    }
}

/// JavaScript-friendly `PublicKey` wrapper.
///
/// For verification without the private key.
#[wasm_bindgen]
pub struct JsPublicKey {
    inner: PublicKey,
}

#[wasm_bindgen]
impl JsPublicKey {
    /// Create from hex string.
    #[wasm_bindgen(js_name = fromHex)]
    pub fn from_hex(hex: &str) -> Result<JsPublicKey, JsError> {
        PublicKey::from_hex(hex)
            .map(|pk| JsPublicKey { inner: pk })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Create from bytes (must be exactly 32 bytes for Ed25519).
    #[wasm_bindgen(js_name = fromBytes)]
    pub fn from_bytes(bytes: &[u8]) -> Result<JsPublicKey, JsError> {
        PublicKey::new(Algorithm::Ed25519, bytes)
            .map(|pk| JsPublicKey { inner: pk })
            .map_err(|e| JsError::new(&e.to_string()))
    }

    /// Get as hex string.
    #[wasm_bindgen(js_name = toHex)]
    pub fn to_hex(&self) -> String {
        self.inner.to_hex()
    }

    /// Verify a signature against a message.
    #[wasm_bindgen]
    pub fn verify(&self, message: &[u8], signature_hex: &str) -> bool {
        match crate::Signature::from_hex(signature_hex) {
            Ok(sig) => self.inner.verify(message, &sig).is_ok(),
            Err(_) => false,
        }
    }
}

/// JavaScript-friendly `CapabilityData` wrapper.
#[wasm_bindgen]
pub struct JsCapabilityData {
    inner: CapabilityData,
}

#[wasm_bindgen]
impl JsCapabilityData {
    /// Create a new capability from key, type, and JSON data.
    ///
    /// # Arguments
    /// * `key` - Stable identifier for matching across chain hops
    /// * `capability_type` - Type tag (e.g., "function", "resource")
    /// * `data` - JSON object as a `JsValue` (will be stored as CBOR bytes internally)
    #[wasm_bindgen(constructor)]
    pub fn new(
        key: &str,
        capability_type: &str,
        data: JsValue,
    ) -> Result<JsCapabilityData, JsError> {
        let json_data: serde_json::Value = serde_wasm_bindgen::from_value(data)
            .map_err(|e| JsError::new(&format!("Invalid JSON data: {e}")))?;

        let cap = CapabilityData::from_json(key, capability_type, &json_data)
            .map_err(|e| JsError::new(&format!("Failed to create capability: {e}")))?;

        Ok(JsCapabilityData { inner: cap })
    }

    /// Get the capability key.
    #[wasm_bindgen]
    pub fn key(&self) -> String {
        self.inner.key().to_string()
    }

    /// Get the capability type.
    #[wasm_bindgen(js_name = capabilityType)]
    pub fn capability_type(&self) -> String {
        self.inner.capability_type().to_string()
    }

    /// Get the data as a JavaScript object.
    /// Decodes internal CBOR bytes back to JSON for JS consumption.
    #[wasm_bindgen]
    pub fn data(&self) -> Result<JsValue, JsError> {
        let json = self
            .inner
            .to_json()
            .map_err(|e| JsError::new(&format!("Failed to decode capability data: {e}")))?;
        serde_wasm_bindgen::to_value(&json)
            .map_err(|e| JsError::new(&format!("Failed to convert data: {e}")))
    }

    /// Get the raw CBOR bytes.
    #[wasm_bindgen(js_name = asBytes)]
    pub fn as_bytes(&self) -> Vec<u8> {
        self.inner.as_bytes().to_vec()
    }

    /// Convert to JSON string representation.
    #[wasm_bindgen(js_name = toJson)]
    pub fn to_json(&self) -> Result<String, JsError> {
        let json = self
            .inner
            .to_json()
            .map_err(|e| JsError::new(&format!("Failed to decode capability data: {e}")))?;
        serde_json::to_string(&json)
            .map_err(|e| JsError::new(&format!("JSON serialization failed: {e}")))
    }
}

/// JavaScript-friendly PCA (PIC Causal Authority) wrapper.
#[wasm_bindgen]
pub struct JsPca {
    inner: Pca,
}

#[wasm_bindgen]
impl JsPca {
    /// Get the PCA hash as hex string.
    #[wasm_bindgen(js_name = hashHex)]
    pub fn hash_hex(&self) -> Result<String, JsError> {
        self.inner
            .try_hash()
            .map(|hash| hash.to_hex())
            .map_err(|e| JsError::new(&format!("Failed to hash PCA: {e}")))
    }

    /// Get the PCA hash as bytes.
    #[wasm_bindgen(js_name = hashBytes)]
    pub fn hash_bytes(&self) -> Result<Vec<u8>, JsError> {
        self.inner
            .try_hash()
            .map(|hash| hash.as_bytes().to_vec())
            .map_err(|e| JsError::new(&format!("Failed to hash PCA: {e}")))
    }

    /// Verify the PCA signature.
    #[wasm_bindgen(js_name = verifySignature)]
    pub fn verify_signature(&self) -> bool {
        self.inner.try_verify_signature().is_ok()
    }

    /// Check if this is a root PCA (no parent).
    #[wasm_bindgen(js_name = isRoot)]
    pub fn is_root(&self) -> bool {
        self.inner.is_root()
    }

    /// Get the issuer's public key as hex.
    #[wasm_bindgen(js_name = issuerHex)]
    pub fn issuer_hex(&self) -> String {
        self.inner.issuer().to_hex()
    }

    /// Get the designated executor's public key as hex (if it's a direct public key).
    ///
    /// Returns None if the executor is a Characteristic or CTA reference.
    #[wasm_bindgen(js_name = designatedExecutorHex)]
    pub fn designated_executor_hex(&self) -> Option<String> {
        self.inner
            .designated_executor()
            .as_public_key()
            .map(super::identity::PublicKey::to_hex)
    }

    /// Get the designated executor as a JSON object.
    ///
    /// Returns an object with `type` and `value` fields:
    /// - `{type: "pubkey", value: "ed25519:..."}` for public key executors
    /// - `{type: "characteristic", value: {char_type: "...", value: "..."}}` for characteristics
    /// - `{type: "cta_ref", value: {cta_key: "ed25519:..."}}` for CTA references
    #[wasm_bindgen(js_name = designatedExecutor)]
    pub fn designated_executor(&self) -> Result<JsValue, JsError> {
        let exec = self.inner.designated_executor();
        serde_wasm_bindgen::to_value(exec)
            .map_err(|e| JsError::new(&format!("Failed to serialize executor: {e}")))
    }

    /// Get the previous PCA hash as hex (if not root).
    #[wasm_bindgen(js_name = prevHashHex)]
    pub fn prev_hash_hex(&self) -> Option<String> {
        self.inner.prev_hash().map(super::hash::PcaHash::to_hex)
    }

    /// Get the root transaction hash as hex.
    #[wasm_bindgen(js_name = rootHashHex)]
    pub fn root_hash_hex(&self) -> Option<String> {
        self.inner.root_hash().map(super::hash::PcaHash::to_hex)
    }

    /// Get the expiry timestamp as ISO 8601 string.
    #[wasm_bindgen(js_name = expiresAt)]
    pub fn expires_at(&self) -> String {
        self.inner.expires_at().to_rfc3339()
    }

    /// Check if the PCA is expired.
    #[wasm_bindgen(js_name = isExpired)]
    pub fn is_expired(&self) -> bool {
        self.inner.is_expired()
    }

    /// Check if expired at a specific time (ISO 8601 string).
    #[wasm_bindgen(js_name = isExpiredAt)]
    pub fn is_expired_at(&self, time_iso: &str) -> Result<bool, JsError> {
        let time = chrono::DateTime::parse_from_rfc3339(time_iso)
            .map_err(|e| JsError::new(&format!("Invalid timestamp: {e}")))?
            .with_timezone(&Utc);
        Ok(self.inner.is_expired_at(time))
    }

    /// Get capabilities as JSON array.
    /// Each capability includes key, type, and decoded data.
    #[wasm_bindgen]
    pub fn capabilities(&self) -> Result<JsValue, JsError> {
        let caps: Result<Vec<serde_json::Value>, _> = self
            .inner
            .capabilities()
            .iter()
            .map(|cap| {
                let data = cap.to_json()?;
                Ok(serde_json::json!({
                    "key": cap.key(),
                    "type": cap.capability_type(),
                    "data": data
                }))
            })
            .collect();

        let caps = caps.map_err(|e: crate::Error| {
            JsError::new(&format!("Failed to decode capability: {e}"))
        })?;

        serde_wasm_bindgen::to_value(&caps)
            .map_err(|e| JsError::new(&format!("Failed to convert capabilities: {e}")))
    }

    /// Serialize to CBOR bytes.
    #[wasm_bindgen(js_name = toCbor)]
    pub fn to_cbor(&self) -> Result<Vec<u8>, JsError> {
        self.inner
            .to_cbor()
            .map_err(|e| JsError::new(&format!("CBOR serialization failed: {e}")))
    }

    /// Deserialize from CBOR bytes.
    #[wasm_bindgen(js_name = fromCbor)]
    pub fn from_cbor(data: &[u8]) -> Result<JsPca, JsError> {
        Pca::from_cbor(data)
            .map(|pca| JsPca { inner: pca })
            .map_err(|e| JsError::new(&format!("CBOR deserialization failed: {e}")))
    }

    /// Get the protocol version.
    #[wasm_bindgen]
    pub fn version(&self) -> String {
        self.inner.version().to_string()
    }
}

/// JavaScript-friendly PCA builder.
#[wasm_bindgen]
pub struct JsPcaBuilder {
    inner: PcaBuilder,
}

#[wasm_bindgen]
#[allow(clippy::return_self_not_must_use)] // Builder pattern for JS doesn't need #[must_use]
impl JsPcaBuilder {
    /// Create a new PCA builder with default protocol version.
    #[wasm_bindgen(constructor)]
    pub fn new() -> JsPcaBuilder {
        JsPcaBuilder {
            inner: PcaBuilder::new().version(PROTOCOL_VERSION),
        }
    }

    /// Set a custom protocol version.
    #[wasm_bindgen]
    pub fn version(mut self, major: u32, minor: u32) -> JsPcaBuilder {
        self.inner = self.inner.version(Version::new(major, minor));
        self
    }

    /// Add a capability.
    #[wasm_bindgen(js_name = addCapability)]
    pub fn add_capability(mut self, cap: &JsCapabilityData) -> JsPcaBuilder {
        self.inner = self.inner.add_capability(cap.inner.clone());
        self
    }

    /// Set the designated executor by hex public key.
    #[wasm_bindgen(js_name = designatedExecutorHex)]
    pub fn designated_executor_hex(
        mut self,
        public_key_hex: &str,
    ) -> Result<JsPcaBuilder, JsError> {
        let pk = PublicKey::from_hex(public_key_hex)
            .map_err(|e| JsError::new(&format!("Invalid public key: {e}")))?;
        self.inner = self.inner.designated_executor(pk);
        Ok(self)
    }

    /// Set the designated executor from a `JsIdentity`.
    #[wasm_bindgen(js_name = designatedExecutor)]
    pub fn designated_executor(mut self, identity: &JsIdentity) -> JsPcaBuilder {
        self.inner = self.inner.designated_executor(identity.inner.public_key());
        self
    }

    /// Set the previous PCA hash (for chain linking).
    #[wasm_bindgen(js_name = prevHashHex)]
    pub fn prev_hash_hex(mut self, hash_hex: &str) -> Result<JsPcaBuilder, JsError> {
        let hash =
            PcaHash::from_hex(hash_hex).map_err(|e| JsError::new(&format!("Invalid hash: {e}")))?;
        self.inner = self.inner.prev_hash(hash);
        Ok(self)
    }

    /// Set the root transaction hash (required for non-root PCAs).
    #[wasm_bindgen(js_name = rootHashHex)]
    pub fn root_hash_hex(mut self, hash_hex: &str) -> Result<JsPcaBuilder, JsError> {
        let hash =
            PcaHash::from_hex(hash_hex).map_err(|e| JsError::new(&format!("Invalid hash: {e}")))?;
        self.inner = self.inner.root_hash(hash);
        Ok(self)
    }

    /// Set the previous PCA hash from a `JsPca`.
    #[wasm_bindgen(js_name = prevPca)]
    pub fn prev_pca(mut self, pca: &JsPca) -> Result<JsPcaBuilder, JsError> {
        let hash = pca
            .inner
            .try_hash()
            .map_err(|e| JsError::new(&format!("Failed to hash PCA: {e}")))?;
        let root_hash = pca.inner.root_hash().copied().unwrap_or(hash);
        self.inner = self.inner.prev_hash(hash).root_hash(root_hash);
        Ok(self)
    }

    /// Set expiry as ISO 8601 timestamp.
    #[wasm_bindgen(js_name = expiresAt)]
    pub fn expires_at(mut self, iso_timestamp: &str) -> Result<JsPcaBuilder, JsError> {
        let time = chrono::DateTime::parse_from_rfc3339(iso_timestamp)
            .map_err(|e| JsError::new(&format!("Invalid timestamp: {e}")))?
            .with_timezone(&Utc);
        self.inner = self.inner.expires_at(time);
        Ok(self)
    }

    /// Set expiry to N hours from now.
    #[wasm_bindgen(js_name = expiresInHours)]
    pub fn expires_in_hours(mut self, hours: i64) -> JsPcaBuilder {
        self.inner = self.inner.expires_at(Utc::now() + Duration::hours(hours));
        self
    }

    /// Set expiry to N minutes from now.
    #[wasm_bindgen(js_name = expiresInMinutes)]
    pub fn expires_in_minutes(mut self, minutes: i64) -> JsPcaBuilder {
        self.inner = self
            .inner
            .expires_at(Utc::now() + Duration::minutes(minutes));
        self
    }

    /// Set expiry to N seconds from now.
    #[wasm_bindgen(js_name = expiresInSeconds)]
    pub fn expires_in_seconds(mut self, seconds: i64) -> JsPcaBuilder {
        self.inner = self
            .inner
            .expires_at(Utc::now() + Duration::seconds(seconds));
        self
    }

    /// Set an optional JSON payload.
    #[wasm_bindgen]
    pub fn payload(mut self, data: JsValue) -> Result<JsPcaBuilder, JsError> {
        let json_data: serde_json::Value = serde_wasm_bindgen::from_value(data)
            .map_err(|e| JsError::new(&format!("Invalid JSON payload: {e}")))?;
        self.inner = self
            .inner
            .payload(&json_data)
            .map_err(|e| JsError::new(&format!("Failed to encode payload: {e}")))?;
        Ok(self)
    }

    /// Build and sign the PCA.
    #[wasm_bindgen(js_name = buildAndSign)]
    pub fn build_and_sign(self, signer: &JsIdentity) -> Result<JsPca, JsError> {
        self.inner
            .build_and_sign(&signer.inner)
            .map(|pca| JsPca { inner: pca })
            .map_err(|e| JsError::new(&format!("Failed to build PCA: {e}")))
    }
}

impl Default for JsPcaBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Get the current protocol version as a string.
#[wasm_bindgen(js_name = protocolVersion)]
pub fn protocol_version() -> String {
    PROTOCOL_VERSION.to_string()
}
