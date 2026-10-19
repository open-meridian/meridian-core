//! RFC 8785 canonical JSON, and the hash over it that names an answer (the
//! one-API spec's Q7, ruled 2026-10-12: "SHA-256 over the canonical JSON of
//! the inputs").
//!
//! The JSON hashed here holds no floating-point number: a decimal is a
//! string in its stated form (`"227.50"`), and a 64-bit integer a string as
//! protobuf's JSON writes one, so RFC 8785's number rule never has to round
//! anything. A whole number is written bare only below 2^53. The same rules
//! as the SDK's reference (`meridian.analytics._canonical`), byte for byte.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// The largest whole number written bare, as a double holds it exactly.
const EXACT: u64 = 1 << 53;

/// Why a value has no canonical form here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotCanonical(pub String);

impl std::fmt::Display for NotCanonical {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotCanonical {}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

fn write(value: &Value, out: &mut String) -> Result<(), NotCanonical> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            let bare = if let Some(whole) = number.as_u64() {
                (whole < EXACT).then(|| whole.to_string())
            } else if let Some(whole) = number.as_i64() {
                (whole.unsigned_abs() < EXACT).then(|| whole.to_string())
            } else {
                return Err(NotCanonical(format!(
                    "{number} is a float, and canonical JSON here carries none: \
                     write a decimal as a string"
                )));
            };
            match bare {
                Some(text) => out.push_str(&text),
                None => {
                    return Err(NotCanonical(format!(
                        "{number} is past what RFC 8785 writes exactly; \
                         write a 64-bit integer as a string"
                    )))
                }
            }
        }
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Value::Object(members) => {
            // RFC 8785 3.2.3: members sorted by their names' UTF-16 code
            // units, which a sort by code points would get wrong past U+FFFF.
            let mut names: Vec<(&String, Vec<u16>)> = members
                .keys()
                .map(|name| (name, name.encode_utf16().collect()))
                .collect();
            names.sort_by(|a, b| a.1.cmp(&b.1));
            out.push('{');
            for (i, (name, _)) in names.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(name, out);
                out.push(':');
                write(&members[*name], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

/// `value` as RFC 8785 canonical JSON, UTF-8.
pub fn canonical(value: &Value) -> Result<String, NotCanonical> {
    let mut out = String::new();
    write(value, &mut out)?;
    Ok(out)
}

/// `sha256:` and the lower-case hex of SHA-256 over bytes.
pub fn sha256_name(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// The answer's hash (Q7): SHA-256 over the canonical JSON of the inputs.
pub fn answer_hash(inputs: &Value) -> Result<String, NotCanonical> {
    Ok(sha256_name(canonical(inputs)?.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn members_sort_by_utf16_and_strings_escape_as_ecmascript() {
        // U+20AC sorts before U+1F600 by UTF-16 code units (0x20AC < 0xD83D);
        // by code points it would be the other way round, as here too, but
        // U+FF61 shows the difference: 0xFF61 > 0xD83D.
        let value = json!({"b": 1, "a": [true, null, "x"], "\u{ff61}": 0, "\u{1f600}": 0});
        assert_eq!(
            canonical(&value).unwrap(),
            "{\"a\":[true,null,\"x\"],\"b\":1,\"\u{1f600}\":0,\"\u{ff61}\":0}"
        );
        assert_eq!(
            canonical(&json!("q\"\\\u{8}\u{c}\n\r\t\u{1}\u{7f}é")).unwrap(),
            "\"q\\\"\\\\\\b\\f\\n\\r\\t\\u0001\u{7f}é\""
        );
        assert!(canonical(&json!(1.5)).is_err());
        assert!(canonical(&json!(1u64 << 53)).is_err());
        assert_eq!(
            canonical(&json!(-((1i64 << 53) - 1))).unwrap(),
            "-9007199254740991"
        );
        assert!(answer_hash(&json!({}))
            .unwrap()
            .starts_with("sha256:44136fa3"));
    }
}
