/**
 * PCA Creation Utilities for JavaScript test harnesses.
 *
 * Creates real Ed25519-signed PCAs matching the Rust wire format.
 * Uses Node.js crypto module for Ed25519 operations.
 */

import { createPrivateKey, sign, generateKeyPairSync } from "crypto";

// =============================================================================
// CBOR Encoding (matches Rust's ciborium format)
// =============================================================================

function cborEncodeUint(majorType, n) {
  if (n < 24) {
    return Buffer.from([majorType | n]);
  } else if (n < 256) {
    return Buffer.from([majorType | 24, n]);
  } else if (n < 65536) {
    const buf = Buffer.alloc(3);
    buf[0] = majorType | 25;
    buf.writeUInt16BE(n, 1);
    return buf;
  } else if (n < 4294967296) {
    const buf = Buffer.alloc(5);
    buf[0] = majorType | 26;
    buf.writeUInt32BE(n, 1);
    return buf;
  } else {
    const buf = Buffer.alloc(9);
    buf[0] = majorType | 27;
    buf.writeBigUInt64BE(BigInt(n), 1);
    return buf;
  }
}

function cborEncodeText(s) {
  const data = Buffer.from(s, "utf8");
  return Buffer.concat([cborEncodeUint(0x60, data.length), data]);
}

function cborEncodeBytes(data) {
  return Buffer.concat([cborEncodeUint(0x40, data.length), data]);
}

function cborEncodeArray(items) {
  const header = cborEncodeUint(0x80, items.length);
  return Buffer.concat([header, ...items]);
}

function cborEncodeMap(pairs) {
  // Preserve insertion order (matches Rust's serde behavior)
  const header = cborEncodeUint(0xa0, pairs.length);
  const encoded = pairs.map(([k, v]) => Buffer.concat([k, v]));
  return Buffer.concat([header, ...encoded]);
}

function cborEncode(value) {
  if (typeof value === "boolean") {
    return Buffer.from([value ? 0xf5 : 0xf4]);
  } else if (typeof value === "number" && Number.isInteger(value)) {
    if (value >= 0) {
      return cborEncodeUint(0x00, value);
    } else {
      return cborEncodeUint(0x20, -1 - value);
    }
  } else if (typeof value === "string") {
    return cborEncodeText(value);
  } else if (Buffer.isBuffer(value)) {
    return cborEncodeBytes(value);
  } else if (Array.isArray(value)) {
    const items = value.map((item) => cborEncode(item));
    return cborEncodeArray(items);
  } else if (value === null || value === undefined) {
    return Buffer.from([0xf6]);
  } else if (typeof value === "object") {
    const pairs = Object.entries(value).map(([k, v]) => [
      cborEncodeText(k),
      cborEncode(v),
    ]);
    return cborEncodeMap(pairs);
  } else {
    throw new Error(`Cannot encode ${typeof value} to CBOR`);
  }
}

// =============================================================================
// EphemeralAuthority - Creates test Ed25519 keypairs and signs PCAs
// =============================================================================

export class EphemeralAuthority {
  constructor() {
    const { privateKey, publicKey } = generateKeyPairSync("ed25519");
    this._privateKey = privateKey;
    this._publicKey = publicKey;
    this._publicKeyBytes = publicKey.export({ type: "spki", format: "der" }).slice(-32);
  }

  /**
   * Get public key in "ed25519:hex" format.
   */
  publicKeyHex() {
    return `ed25519:${this._publicKeyBytes.toString("hex")}`;
  }

  /**
   * Get raw public key bytes (32 bytes).
   */
  publicKeyBytes() {
    return this._publicKeyBytes;
  }

  /**
   * Sign a message and return 64-byte signature.
   */
  sign(message) {
    return sign(null, message, this._privateKey);
  }

  /**
   * Create a signed PCA with the specified capabilities.
   *
   * @param {string[]} capabilities - Capability patterns like "tool_call:**"
   * @param {number} expiresInSecs - Time until expiry (default: 3600)
   * @returns {Buffer} CBOR-encoded PCA
   */
  createPca(capabilities = ["tool_call:**"], expiresInSecs = 3600) {
    // Calculate expiry (RFC3339 format with +00:00)
    const expiresAt = new Date(Date.now() + expiresInSecs * 1000);
    const expiresStr = expiresAt.toISOString().replace("Z", "+00:00").replace(/\.\d{3}/, "");

    const issuerStr = this.publicKeyHex();
    const executorKeyBytes = this._publicKeyBytes;
    const executorWire = [0, executorKeyBytes]; // 0 = Ed25519

    // Create capability list matching Rust's CapabilityData
    const capList = capabilities.map((cap, i) => {
      let pattern = cap;
      if (cap.startsWith("tool_call:")) {
        pattern = cap.slice("tool_call:".length);
      }

      // Payload is CBOR-encoded ToolCallCap {tool: pattern, constraints: []}
      const payloadCbor = cborEncode({ tool: pattern, constraints: [] });

      return {
        key: `cap:${i}`,
        type: "tool-call",
        data: payloadCbor,
      };
    });

    // Build signable content (matches Rust's PcaSignable field order)
    const signable = {
      version: [0, 1],
      capabilities: capList,
      designated_executor: {
        type: "pubkey",
        value: executorWire,
      },
      expires_at: expiresStr,
      issuer: issuerStr,
    };

    // Encode and sign
    const signableCbor = cborEncode(signable);
    const signature = this.sign(signableCbor);
    const signatureStr = `ed25519:${signature.toString("hex")}`;

    // Build full PCA wire format
    const pcaWire = {
      version: [0, 1],
      capabilities: capList,
      designated_executor: {
        type: "pubkey",
        value: executorWire,
      },
      expires_at: expiresStr,
      issuer: issuerStr,
      signature: signatureStr,
    };

    return cborEncode(pcaWire);
  }
}

// =============================================================================
// Runtime Creation Helper
// =============================================================================

/**
 * Create a runtime with a real signed PCA.
 *
 * @param {object} exports - WASM exports
 * @param {WebAssembly.Memory} memory - WASM memory
 * @returns {{rtId: bigint, authority: EphemeralAuthority}}
 */
export function createRuntimeWithPca(exports, memory) {
  const encoder = new TextEncoder();

  // Generate ephemeral authority and PCA
  const authority = new EphemeralAuthority();
  const pca = authority.createPca(["tool_call:**"]);
  const trustedAuthorities = [authority.publicKeyHex()];

  // Set trusted authorities first
  const authJson = JSON.stringify(trustedAuthorities);
  const authBytes = encoder.encode(authJson);
  const authPtr = 512;
  new Uint8Array(memory.buffer, authPtr, authBytes.length).set(authBytes);

  const setResult = exports.set_trusted_authorities(authPtr, authBytes.length);
  // setResult returns the count of authorities set, or 0 on error
  if (setResult === 0) {
    throw new Error(`set_trusted_authorities failed (returned 0)`);
  }

  // Create runtime with PCA
  const pcaPtr = 1024;
  new Uint8Array(memory.buffer, pcaPtr, pca.length).set(pca);

  const rtId = exports.runtime_new(pcaPtr, pca.length);
  if (rtId === 0n || rtId === 0) {
    // Try to get error
    if (exports.get_last_error) {
      const errPtr = 4096;
      const errLen = exports.get_last_error(errPtr, 1024);
      if (errLen > 0) {
        const decoder = new TextDecoder();
        const errMsg = decoder.decode(new Uint8Array(memory.buffer, errPtr, errLen));
        throw new Error(`Failed to create runtime: ${errMsg}`);
      }
    }
    throw new Error("Failed to create runtime (no error message)");
  }

  return { rtId, authority };
}

// =============================================================================
// Exports
// =============================================================================

export { cborEncode };
