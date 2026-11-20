//! Minimal JSON writer with Python `json.dumps`-compatible formatting.
//!
//! Field order is preserved and separators are `", "` / `": "`, matching the
//! original CLI's machine output byte-for-byte for ASCII payloads. Non-ASCII
//! characters are escaped as `\uXXXX` (`ensure_ascii=True`).

use std::fmt::Write as _;

/// A JSON value with deterministic, insertion-ordered objects.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// A signed integer.
    Int(i64),
    /// A string, escaped like Python's default encoder.
    Str(String),
    /// An array.
    Array(Vec<Self>),
    /// An object whose key order is preserved.
    Object(Vec<(String, Self)>),
}

impl Json {
    /// Render with Python's default separators and ASCII escaping.
    pub fn to_python_string(&self) -> String {
        let mut out = String::new();
        self.write_to(&mut out);
        out
    }

    fn write_to(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Self::Int(value) => {
                let _ = write!(out, "{value}");
            }
            Self::Str(value) => escape_into(value, out),
            Self::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    item.write_to(out);
                }
                out.push(']');
            }
            Self::Object(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    escape_into(key, out);
                    out.push_str(": ");
                    value.write_to(out);
                }
                out.push('}');
            }
        }
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<i64> for Json {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<i32> for Json {
    fn from(value: i32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u32> for Json {
    fn from(value: u32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u64> for Json {
    fn from(value: u64) -> Self {
        Self::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Self::Int(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl<T: Into<Self>> From<Option<T>> for Json {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Null, Into::into)
    }
}

/// Escape one string exactly like Python's `json.dumps(..., ensure_ascii=True)`.
fn escape_into(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ('\u{20}'..='\u{7e}').contains(&ch) => out.push(ch),
            ch => {
                let mut units = [0_u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_with_python_separators_and_order() {
        let value = Json::Object(vec![
            ("event".to_owned(), Json::from("poll")),
            ("temp_c".to_owned(), Json::from(63_i32)),
            ("fan_pct".to_owned(), Json::from(Some(12_i32))),
            ("mode".to_owned(), Json::from("manual")),
            (
                "fan_rpms".to_owned(),
                Json::Array(vec![Json::Array(vec![
                    Json::from("FAN_1"),
                    Json::from(4320_i32),
                ])]),
            ),
        ]);
        assert_eq!(
            value.to_python_string(),
            r#"{"event": "poll", "temp_c": 63, "fan_pct": 12, "mode": "manual", "fan_rpms": [["FAN_1", 4320]]}"#
        );
    }

    #[test]
    fn escapes_like_python_ensure_ascii() {
        assert_eq!(
            Json::from("a\nb\t\"c\\").to_python_string(),
            r#""a\nb\t\"c\\""#
        );
        assert_eq!(Json::from("é").to_python_string(), r#""\u00e9""#);
        assert_eq!(Json::from("\u{7f}").to_python_string(), r#""\u007f""#);
        assert_eq!(Json::from("😀").to_python_string(), r#""\ud83d\ude00""#);
    }

    #[test]
    fn null_from_none() {
        assert_eq!(Json::from(Option::<i32>::None).to_python_string(), "null");
        assert_eq!(Json::from(false).to_python_string(), "false");
    }
}
