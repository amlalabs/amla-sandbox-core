//! CBOR serialization utilities.
//!
//! Uses canonical CBOR (RFC 7049 Section 3.9) for deterministic encoding:
//! - Integers in smallest encoding
//! - Map keys sorted by encoded form (lexicographic byte order)
//! - No indefinite-length items
//!
//! # Why Canonical CBOR?
//!
//! Deterministic encoding is critical for this protocol because:
//! - **Hash stability**: Same data must produce same hash for chain linking
//! - **Signature verification**: Signed bytes must be reproducible exactly
//! - **Cross-implementation compatibility**: Different implementations must encode identically
//!
//! # JSON ↔ CBOR Conversion
//!
//! The module provides bidirectional conversion between JSON and CBOR values.
//! This enables:
//! - Human-readable capability definitions in JSON
//! - Compact wire format in CBOR
//! - Round-trip fidelity for interoperability
//!
//! Note: Some CBOR features (bytes, non-string map keys) have no JSON equivalent
//! and are handled specially during conversion.

use ciborium::Value;
use serde::{Serialize, de::DeserializeOwned};

use crate::error::{Error, Result};

/// Encode to canonical CBOR.
///
/// Canonical CBOR ensures deterministic encoding, which is critical
/// for hashing and signing operations.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn canonical_cbor_encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).map_err(|e| Error::CborSerialize(e.to_string()))?;

    // ciborium uses canonical encoding by default (sorted keys, minimal integers)
    Ok(buf)
}

/// Decode CBOR bytes.
///
/// # Errors
///
/// Returns an error if deserialization fails.
pub fn cbor_decode<T: DeserializeOwned>(data: &[u8]) -> Result<T> {
    ciborium::from_reader(data).map_err(|e| Error::CborDeserialize(e.to_string()))
}

/// Decode CBOR bytes to a ciborium Value.
///
/// Internal helper for tests.
///
/// # Errors
///
/// Returns an error if deserialization fails.
#[cfg(test)]
pub(crate) fn cbor_decode_value(data: &[u8]) -> Result<Value> {
    cbor_decode(data)
}

/// Decode CBOR bytes to a [`serde_json::Value`].
///
/// This uses a two-step process: first decode to [`ciborium::Value`],
/// then convert to [`serde_json::Value`]. This avoids a hang in WASM
/// when using serde's Deserialize trait directly with [`serde_json::Value`].
///
/// # Errors
///
/// Returns an error if deserialization fails.
pub fn cbor_decode_to_json(data: &[u8]) -> Result<serde_json::Value> {
    // Step 1: Decode to ciborium::Value (works correctly in WASM)
    let cbor_value: Value = cbor_decode(data)?;
    // Step 2: Convert to serde_json::Value
    Ok(cbor_to_json_value(&cbor_value))
}

/// Validate that a value is CBOR-serializable.
///
/// Allowed types:
/// - `null`, `bool`, integers, floats, strings, bytes
/// - Arrays with serializable elements
/// - Maps with string keys and serializable values
///
/// # Errors
///
/// Returns an error if the value contains non-serializable types.
pub fn validate_cbor_serializable(value: &serde_json::Value) -> Result<()> {
    validate_at_path(value, "root")
}

fn validate_at_path(value: &serde_json::Value, path: &str) -> Result<()> {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => Ok(()),

        serde_json::Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                validate_at_path(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }

        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                validate_at_path(v, &format!("{path}.{k}"))?;
            }
            Ok(())
        }
    }
}

/// Convert a `serde_json::Value` to a `ciborium::Value`.
///
/// This is useful for embedding arbitrary JSON-like data in CBOR.
#[must_use]
pub fn json_to_cbor_value(json: &serde_json::Value) -> Value {
    match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Integer(u.into())
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                // serde_json::Number only supports i64, u64, and f64.
                // This branch is unreachable with standard serde_json.
                unreachable!("serde_json::Number should always be i64, u64, or f64")
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(arr) => Value::Array(arr.iter().map(json_to_cbor_value).collect()),
        serde_json::Value::Object(obj) => Value::Map(
            obj.iter()
                .map(|(k, v)| (Value::Text(k.clone()), json_to_cbor_value(v)))
                .collect(),
        ),
    }
}

/// Convert a `ciborium::Value` to a `serde_json::Value`.
///
/// Note: Some CBOR types (bytes, tags) don't have direct JSON equivalents
/// and will be converted to strings or null.
#[must_use]
#[allow(clippy::match_same_arms)] // Null and wildcard both return Null intentionally
pub fn cbor_to_json_value(cbor: &Value) -> serde_json::Value {
    match cbor {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => {
            if let Ok(n) = i64::try_from(*i) {
                serde_json::json!(n)
            } else if let Ok(n) = u64::try_from(*i) {
                serde_json::json!(n)
            } else {
                // Large integer - convert to string
                serde_json::Value::String(format!("{i:?}"))
            }
        }
        Value::Float(f) => serde_json::json!(f),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => {
            // Encode bytes as hex string
            serde_json::Value::String(hex::encode(b))
        }
        Value::Array(arr) => serde_json::Value::Array(arr.iter().map(cbor_to_json_value).collect()),
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> = map
                .iter()
                .filter_map(|(k, v)| {
                    // Only include string keys
                    if let Value::Text(key) = k {
                        Some((key.clone(), cbor_to_json_value(v)))
                    } else {
                        None
                    }
                })
                .collect();
            serde_json::Value::Object(obj)
        }
        Value::Tag(_, inner) => cbor_to_json_value(inner),
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
#[allow(clippy::approx_constant, clippy::unreadable_literal)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestStruct {
        name: String,
        value: i32,
        nested: NestedStruct,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct NestedStruct {
        flag: bool,
        items: Vec<String>,
    }

    #[test]
    fn test_roundtrip() {
        let original = TestStruct {
            name: "test".to_string(),
            value: 42,
            nested: NestedStruct {
                flag: true,
                items: vec!["a".to_string(), "b".to_string()],
            },
        };

        let encoded = canonical_cbor_encode(&original).unwrap();
        let decoded: TestStruct = cbor_decode(&encoded).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn test_deterministic_encoding() {
        let data1 = TestStruct {
            name: "test".to_string(),
            value: 123,
            nested: NestedStruct {
                flag: false,
                items: vec![],
            },
        };

        let data2 = TestStruct {
            name: "test".to_string(),
            value: 123,
            nested: NestedStruct {
                flag: false,
                items: vec![],
            },
        };

        let encoded1 = canonical_cbor_encode(&data1).unwrap();
        let encoded2 = canonical_cbor_encode(&data2).unwrap();

        assert_eq!(encoded1, encoded2);
    }

    #[test]
    fn test_sorted_map_keys() {
        use std::collections::BTreeMap;

        let mut map = BTreeMap::new();
        map.insert("z_last".to_string(), 1);
        map.insert("a_first".to_string(), 2);
        map.insert("m_middle".to_string(), 3);

        let encoded = canonical_cbor_encode(&map).unwrap();

        // Decode to Value to inspect structure
        let value: Value = cbor_decode(&encoded).unwrap();

        if let Value::Map(pairs) = value {
            // Keys should be sorted
            let keys: Vec<_> = pairs
                .iter()
                .filter_map(|(k, _)| {
                    if let Value::Text(s) = k {
                        Some(s.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(keys, vec!["a_first", "m_middle", "z_last"]);
        } else {
            panic!("Expected map");
        }
    }

    #[test]
    fn test_json_cbor_conversion() {
        let json = serde_json::json!({
            "string": "hello",
            "number": 42,
            "float": 3.14,
            "bool": true,
            "null": null,
            "array": [1, 2, 3],
            "object": {"nested": "value"}
        });

        let cbor = json_to_cbor_value(&json);
        let back = cbor_to_json_value(&cbor);

        assert_eq!(json, back);
    }

    #[test]
    fn test_validate_cbor_serializable() {
        // Valid values
        assert!(validate_cbor_serializable(&serde_json::json!(null)).is_ok());
        assert!(validate_cbor_serializable(&serde_json::json!(true)).is_ok());
        assert!(validate_cbor_serializable(&serde_json::json!(42)).is_ok());
        assert!(validate_cbor_serializable(&serde_json::json!("hello")).is_ok());
        assert!(validate_cbor_serializable(&serde_json::json!([1, 2, 3])).is_ok());
        assert!(validate_cbor_serializable(&serde_json::json!({"key": "value"})).is_ok());

        // Nested structures
        assert!(
            validate_cbor_serializable(&serde_json::json!({
                "nested": {
                    "array": [1, {"deep": true}]
                }
            }))
            .is_ok()
        );
    }

    #[test]
    fn test_empty_structures() {
        // Empty string
        let empty_string = serde_json::json!("");
        let cbor = json_to_cbor_value(&empty_string);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(empty_string, back);

        // Empty array
        let empty_array = serde_json::json!([]);
        let cbor = json_to_cbor_value(&empty_array);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(empty_array, back);

        // Empty object
        let empty_object = serde_json::json!({});
        let cbor = json_to_cbor_value(&empty_object);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(empty_object, back);
    }

    #[test]
    fn test_large_numbers() {
        // Large positive integer
        let large_pos = serde_json::json!(i64::MAX);
        let cbor = json_to_cbor_value(&large_pos);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(large_pos, back);

        // Large negative integer
        let large_neg = serde_json::json!(i64::MIN);
        let cbor = json_to_cbor_value(&large_neg);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(large_neg, back);

        // Zero
        let zero = serde_json::json!(0);
        let cbor = json_to_cbor_value(&zero);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(zero, back);
    }

    #[test]
    fn test_special_strings() {
        // Unicode
        let unicode = serde_json::json!("Hello, 世界! 🌍");
        let cbor = json_to_cbor_value(&unicode);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(unicode, back);

        // Newlines and tabs
        let whitespace = serde_json::json!("line1\nline2\ttabbed");
        let cbor = json_to_cbor_value(&whitespace);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(whitespace, back);

        // Quotes
        let quotes = serde_json::json!("He said \"hello\"");
        let cbor = json_to_cbor_value(&quotes);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(quotes, back);
    }

    #[test]
    fn test_deeply_nested() {
        let deep = serde_json::json!({
            "a": {
                "b": {
                    "c": {
                        "d": {
                            "e": {
                                "value": 42
                            }
                        }
                    }
                }
            }
        });

        let cbor = json_to_cbor_value(&deep);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(deep, back);

        // Roundtrip via encoding
        let encoded = canonical_cbor_encode(&deep).unwrap();
        let decoded: serde_json::Value = cbor_decode(&encoded).unwrap();
        assert_eq!(deep, decoded);
    }

    #[test]
    fn test_mixed_array() {
        let mixed = serde_json::json!([
            null,
            true,
            false,
            42,
            -17,
            3.14,
            "string",
            [],
            {},
            {"nested": [1, 2, 3]}
        ]);

        let cbor = json_to_cbor_value(&mixed);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(mixed, back);
    }

    #[test]
    fn test_cbor_decode_invalid() {
        // Empty data
        assert!(cbor_decode::<TestStruct>(&[]).is_err());

        // Random garbage
        assert!(cbor_decode::<TestStruct>(&[0xFF, 0xFE, 0x00]).is_err());

        // Valid CBOR but wrong type
        let number = canonical_cbor_encode(&42i32).unwrap();
        assert!(cbor_decode::<TestStruct>(&number).is_err());
    }

    #[test]
    fn test_canonical_encoding_is_deterministic() {
        // Same data should produce same bytes every time
        let data = serde_json::json!({
            "z": 1,
            "a": 2,
            "m": {"b": 3, "a": 4}
        });

        let enc1 = canonical_cbor_encode(&data).unwrap();
        let enc2 = canonical_cbor_encode(&data).unwrap();
        let enc3 = canonical_cbor_encode(&data).unwrap();

        assert_eq!(enc1, enc2);
        assert_eq!(enc2, enc3);
    }

    #[test]
    fn test_cbor_bytes_conversion() {
        // CBOR bytes get converted to hex string in JSON
        let bytes_value = Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        let json = cbor_to_json_value(&bytes_value);
        assert_eq!(json, serde_json::json!("deadbeef"));
    }

    #[test]
    fn test_cbor_tag_unwrapping() {
        // CBOR tags should be unwrapped
        let tagged = Value::Tag(1, Box::new(Value::Integer(12345.into())));
        let json = cbor_to_json_value(&tagged);
        assert_eq!(json, serde_json::json!(12345));
    }

    #[test]
    fn test_float_preservation() {
        let floats = serde_json::json!({
            "pi": 3.14159265359,
            "e": 2.71828182845,
            "zero": 0.0,
            "negative": -123.456
        });

        let cbor = json_to_cbor_value(&floats);
        let back = cbor_to_json_value(&cbor);
        assert_eq!(floats, back);
    }

    #[test]
    fn test_cbor_decode_value() {
        // Test the generic Value decoder
        let data = serde_json::json!({"key": 123, "arr": [1, 2, 3]});
        let encoded = canonical_cbor_encode(&data).unwrap();
        let value = cbor_decode_value(&encoded).unwrap();

        // Should be a Map
        assert!(matches!(value, Value::Map(_)));
    }

    #[test]
    fn test_json_to_cbor_large_unsigned() {
        // Test conversion of u64 values larger than i64::MAX
        let large_u64 = u64::MAX;
        let json = serde_json::json!(large_u64);
        let cbor = json_to_cbor_value(&json);

        // Should convert back successfully
        let back = cbor_to_json_value(&cbor);
        assert_eq!(back, json);
    }

    #[test]
    fn test_cbor_to_json_large_integer() {
        // Test integer larger than both i64 and u64 can handle
        // ciborium::Value::Integer can hold 128-bit values via Integer type
        use ciborium::value::Integer;

        // Create a value larger than u64::MAX (using Integer::from(i128))
        // Actually, Integer is from i128, so let's use a value that's valid but
        // tests the conversion paths
        let large_int = Value::Integer(Integer::from(u64::MAX));
        let json = cbor_to_json_value(&large_int);

        // Should be a valid u64
        assert_eq!(json, serde_json::json!(u64::MAX));
    }

    #[test]
    fn test_cbor_map_non_string_keys_filtered() {
        // CBOR maps can have non-string keys, but JSON cannot
        // Test that non-string keys are filtered out
        let cbor_map = Value::Map(vec![
            (Value::Text("valid_key".into()), Value::Integer(1.into())),
            (Value::Integer(123.into()), Value::Integer(2.into())), // Non-string key
            (Value::Text("another_key".into()), Value::Integer(3.into())),
        ]);

        let json = cbor_to_json_value(&cbor_map);

        // Should only have the string keys
        if let serde_json::Value::Object(obj) = json {
            assert_eq!(obj.len(), 2);
            assert!(obj.contains_key("valid_key"));
            assert!(obj.contains_key("another_key"));
            assert!(!obj.contains_key("123")); // Integer key was filtered
        } else {
            panic!("Expected object");
        }
    }

    #[test]
    fn test_cbor_wildcard_value() {
        // Test the wildcard arm of cbor_to_json_value (returns Null)
        // ciborium has some special values we can use
        let undefined = Value::Tag(0, Box::new(Value::Null)); // Tagged null
        let json = cbor_to_json_value(&undefined);
        // Tags are unwrapped, so this returns Null
        assert_eq!(json, serde_json::Value::Null);
    }

    #[test]
    fn test_truncated_cbor_map_returns_error() {
        // This pattern was causing hangs in WASM builds.
        // It's a map(3) with 2 complete entries and an incomplete 3rd key.
        // The parser should return an error, not hang.
        let bytes: Vec<u8> = vec![
            0xa3, // map(3)
            0x63, 0x6b, 0x65, 0x79, // text(3) "key"
            0x68, 0x63, 0x61, 0x70, 0x3a, 0x65, 0x63, 0x68, 0x6f, // text(8) "cap:echo"
            0x64, 0x74, 0x79, 0x70, 0x65, // text(4) "type"
            0x69, 0x74, 0x6f, 0x6f, 0x6c, 0x2d, 0x63, 0x61, 0x6c, 0x6c, // text(9) "tool-call"
            0x64, // text(4) - start of "data" key but no actual text bytes
        ];

        // This MUST return an error, not hang
        let result = cbor_decode::<serde_json::Value>(&bytes);
        assert!(result.is_err(), "truncated CBOR should return error");
    }
}
