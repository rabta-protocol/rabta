use serde_json::{Map, Number, Value};

/// Canonical JSON for the event digest:
/// exactly the fields `{bin?, collection, created_at, data, expires_at, labels,
/// owner}` — **sorted keys, compact separators**. `id` and `sig` are never in
/// the input. `bin` is optional and omitted from the digest when absent; when
/// present it is an ordered array of blob hashes. Blobs themselves are a
/// separate plane and are never uploaded by the event path.
///
/// serde_json objects are sorted maps, so serialization order is already
/// lexicographic; `to_vec` is compact by default. Integers round-trip exactly.
///
/// Note for client authors: keep `data` free of floating-point numbers whose
/// JS serialization differs from Rust's (`1.0` — JS prints `1`, Rust `1.0`).
/// Use integers and strings in `data`; both canonicalize identically everywhere.
pub fn canonical_value(v: &Value) -> Vec<u8> {
    serde_json::to_vec(v).expect("value serialization cannot fail")
}

/// Build the digest input envelope from the parsed event fields.
/// `labels` keeps the client's array order (arrays are ordered in both
/// canonicalization and JSON, so this is deterministic). `bin` follows the
/// same rule and is inserted only when the envelope carried it.
pub fn digest_value(
    collection: &str,
    created_at: i64,
    expires_at: i64,
    owner: &str,
    labels: &[String],
    data: &Value,
    bin: Option<&[String]>,
) -> Value {
    let mut m = Map::new();
    if let Some(bin) = bin {
        m.insert("bin".into(), serde_json::to_value(bin).unwrap());
    }
    m.insert("collection".into(), Value::String(collection.to_string()));
    m.insert("created_at".into(), Value::Number(Number::from(created_at)));
    m.insert("data".into(), data.clone());
    m.insert("expires_at".into(), Value::Number(Number::from(expires_at)));
    m.insert("labels".into(), serde_json::to_value(labels).unwrap());
    m.insert("owner".into(), Value::String(owner.to_string()));
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorted_keys_compact() {
        let v = json!({"z": 1, "a": {"b": [1, 2], "a": "x"}});
        let b = canonical_value(&v);
        assert_eq!(b, br#"{"a":{"a":"x","b":[1,2]},"z":1}"#);
    }

    #[test]
    fn digest_has_fixed_shape() {
        let v = digest_value(
            "c",
            100,
            200,
            "ed25519:ab",
            &["l1".into()],
            &json!({"m": "hello"}),
            None,
        );
        let s = String::from_utf8(canonical_value(&v)).unwrap();
        assert_eq!(
            s,
            r#"{"collection":"c","created_at":100,"data":{"m":"hello"},"expires_at":200,"labels":["l1"],"owner":"ed25519:ab"}"#
        );
    }

    #[test]
    fn bin_is_digested_only_when_present() {
        let hash = "a".repeat(64);
        let v = digest_value(
            "c",
            100,
            200,
            "ed25519:ab",
            &[],
            &json!({}),
            Some(&[hash.clone()]),
        );
        let s = String::from_utf8(canonical_value(&v)).unwrap();
        assert_eq!(
            s,
            format!(
                r#"{{"bin":["{hash}"],"collection":"c","created_at":100,"data":{{}},"expires_at":200,"labels":[],"owner":"ed25519:ab"}}"#
            )
        );
    }
}
