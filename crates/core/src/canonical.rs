//! JSON Canonicalization Scheme (RFC 8785) for content-addressed IDs.
//!
//! Object keys are sorted by UTF-16 code units, strings use the minimal JSON
//! escaping, and numbers use the ECMAScript `Number.prototype.toString` format.

use serde_json::Value;

/// Serializes `value` in JCS form.
pub fn to_canonical_string(value: &Value) -> String {
    let mut out = String::new();
    write_value(value, &mut out);
    out
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            // Integers within ±2^53 print exactly; larger ones go through the double
            // they denote, as JCS requires, so every implementation agrees on the ID.
            const EXACT: u64 = 1 << 53;
            match (n.as_i64(), n.as_u64()) {
                (Some(i), _) if i.unsigned_abs() <= EXACT => out.push_str(&i.to_string()),
                (_, Some(u)) if u <= EXACT => out.push_str(&u.to_string()),
                _ => out.push_str(&es_number(n.as_f64().expect("JSON numbers are finite"))),
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(v, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Formats a finite double the way ECMAScript's `Number.prototype.toString` does.
fn es_number(f: f64) -> String {
    if f == 0.0 {
        return "0".to_owned();
    }
    let sign = if f < 0.0 { "-" } else { "" };
    // Rust's `{:e}` prints the shortest round-tripping digits, like ECMAScript.
    let sci = format!("{:e}", f.abs());
    let (mantissa, exp) = sci.split_once('e').expect("scientific notation");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp.parse::<i32>().expect("exponent") + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let e = if e >= 0 {
            format!("+{e}")
        } else {
            e.to_string()
        };
        if k == 1 {
            format!("{digits}e{e}")
        } else {
            format!("{}.{}e{e}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sorts_keys_and_drops_whitespace() {
        let v = json!({"b": [1, {"z": null, "a": true}], "a": "x"});
        assert_eq!(
            to_canonical_string(&v),
            r#"{"a":"x","b":[1,{"a":true,"z":null}]}"#
        );
    }

    #[test]
    fn sorts_by_utf16_code_units() {
        // By code point U+FB33 sorts before U+1F600, but by UTF-16 code units
        // U+1F600 (0xD83D 0xDE00) sorts before U+FB33.
        let v = json!({"\u{fb33}": 1, "\u{1f600}": 6, "\u{20ac}": 2, "\r": 3, "1": 4, "\u{80}": 5});
        assert_eq!(
            to_canonical_string(&v),
            "{\"\\r\":3,\"1\":4,\"\u{80}\":5,\"\u{20ac}\":2,\"\u{1f600}\":6,\"\u{fb33}\":1}"
        );
    }

    #[test]
    fn escapes_strings_minimally() {
        let v = json!("a\"b\\c\nd\u{1}e/é");
        assert_eq!(to_canonical_string(&v), "\"a\\\"b\\\\c\\nd\\u0001e/é\"");
    }

    #[test]
    fn formats_numbers_like_ecmascript() {
        let cases = [
            (0.5, "0.5"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (123.456, "123.456"),
            (1.5e-10, "1.5e-10"),
            (4.5e300, "4.5e+300"),
            (-0.0, "0"),
            (333333333.3333333, "333333333.3333333"),
        ];
        for (f, want) in cases {
            assert_eq!(es_number(f), want, "{f}");
        }
        assert_eq!(
            to_canonical_string(&json!([
                1,
                -7,
                9007199254740992u64,
                -9007199254740992i64,
                2.0
            ])),
            "[1,-7,9007199254740992,-9007199254740992,2]"
        );
        // Beyond 2^53, integers canonicalize as the double they denote.
        assert_eq!(
            to_canonical_string(&json!([
                9007199254740993u64,
                -9007199254740993i64,
                18446744073709551615u64
            ])),
            "[9007199254740992,-9007199254740992,18446744073709552000]"
        );
    }
}
