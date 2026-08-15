//! Shared bounded validation for every untrusted MCP message and model.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use std::fmt;

pub const MAX_ITEMS: usize = 256;
pub const MAX_CURSOR_BYTES: usize = 512;
pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
pub const MAX_IDENTIFIER_BYTES: usize = 128;
pub const MAX_METADATA_TEXT_BYTES: usize = 4096;
pub const MAX_MIME_BYTES: usize = 256;
pub const MAX_JSON_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("MCP collection exceeds its item limit")]
    TooManyItems,
    #[error("MCP cursor exceeds its byte limit")]
    CursorTooLarge,
    #[error("MCP URI exceeds its byte limit")]
    UriTooLarge,
    #[error("MCP content exceeds its byte limit")]
    ContentTooLarge,
    #[error("MCP JSON exceeds its nesting limit")]
    TooDeep,
    #[error("MCP cursor is not opaque UTF-8 data")]
    InvalidCursor,
    #[error("MCP value is invalid")]
    InvalidValue,
    #[error("MCP JSON is malformed or has trailing data")]
    Malformed,
}

pub trait ValidateMcp {
    fn validate_mcp(&self) -> Result<(), ValidationError>;
}

impl ValidateMcp for Value {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validate_json(self)
    }
}

pub fn validate_collection_len(len: usize) -> Result<(), ValidationError> {
    (len <= MAX_ITEMS)
        .then_some(())
        .ok_or(ValidationError::TooManyItems)
}

pub fn validate_cursor(cursor: Option<&str>) -> Result<(), ValidationError> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
        return Err(ValidationError::CursorTooLarge);
    }
    if cursor.chars().any(char::is_control) {
        return Err(ValidationError::InvalidCursor);
    }
    Ok(())
}

pub fn validate_uri(uri: &str) -> Result<(), ValidationError> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES {
        Err(ValidationError::UriTooLarge)
    } else {
        Ok(())
    }
}

pub fn validate_metadata_text(text: &str) -> Result<(), ValidationError> {
    (!text.is_empty() && text.len() <= MAX_METADATA_TEXT_BYTES)
        .then_some(())
        .ok_or(ValidationError::InvalidValue)
}

pub fn validate_identifier(text: &str) -> Result<(), ValidationError> {
    (!text.is_empty() && text.len() <= MAX_IDENTIFIER_BYTES && !text.chars().any(char::is_control))
        .then_some(())
        .ok_or(ValidationError::InvalidValue)
}

pub fn validate_mime(text: &str) -> Result<(), ValidationError> {
    (!text.is_empty() && text.len() <= MAX_MIME_BYTES && !text.chars().any(char::is_control))
        .then_some(())
        .ok_or(ValidationError::InvalidValue)
}

pub fn validate_content_text(text: &str) -> Result<(), ValidationError> {
    (text.len() <= MAX_CONTENT_BYTES)
        .then_some(())
        .ok_or(ValidationError::ContentTooLarge)
}

pub fn parse_bounded_json(bytes: &[u8]) -> Result<Value, ValidationError> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = BoundedValueSeed { depth: 0 }
        .deserialize(&mut deserializer)
        .map_err(|error| {
            let message = error.to_string();
            if message.contains("MCP_LIMIT") || message.contains("MCP_DUPLICATE_KEY") {
                ValidationError::InvalidValue
            } else {
                ValidationError::Malformed
            }
        })?;
    deserializer.end().map_err(|_| ValidationError::Malformed)?;
    Ok(value)
}

struct BoundedValueSeed {
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for BoundedValueSeed {
    type Value = Value;
    fn deserialize<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        if self.depth > MAX_JSON_DEPTH {
            return Err(de::Error::custom("MCP_LIMIT_DEPTH"));
        }
        deserializer.deserialize_any(BoundedValueVisitor { depth: self.depth })
    }
}
struct RejectExtraValueSeed;
impl<'de> DeserializeSeed<'de> for RejectExtraValueSeed {
    type Value = ();
    fn deserialize<D>(self, _deserializer: D) -> Result<(), D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Err(de::Error::custom("MCP_LIMIT_ITEMS"))
    }
}

struct BoundedValueVisitor {
    depth: usize,
}
impl<'de> Visitor<'de> for BoundedValueVisitor {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON value")
    }
    fn visit_bool<E>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while values.len() < MAX_ITEMS {
            let Some(value) = access.next_element_seed(BoundedValueSeed {
                depth: self.depth + 1,
            })?
            else {
                return Ok(Value::Array(values));
            };
            values.push(value);
        }
        let _: Option<()> = access.next_element_seed(RejectExtraValueSeed)?;
        Ok(Value::Array(values))
    }
    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = access.next_key::<String>()? {
            if values.len() >= MAX_ITEMS {
                return Err(de::Error::custom("MCP_LIMIT_ITEMS"));
            }
            if values.contains_key(&key) {
                return Err(de::Error::custom("MCP_DUPLICATE_KEY"));
            }
            let value = access.next_value_seed(BoundedValueSeed {
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

pub fn validate_json(value: &Value) -> Result<(), ValidationError> {
    fn walk(value: &Value, depth: usize) -> Result<(), ValidationError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ValidationError::TooDeep);
        }
        match value {
            Value::Array(values) => {
                validate_collection_len(values.len())?;
                values.iter().try_for_each(|value| walk(value, depth + 1))
            }
            Value::Object(values) => {
                validate_collection_len(values.len())?;
                values.values().try_for_each(|value| walk(value, depth + 1))
            }
            _ => Ok(()),
        }
    }
    walk(value, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enforces_collection_cursor_uri_content_and_depth_limits() {
        assert!(validate_collection_len(MAX_ITEMS + 1).is_err());
        assert!(validate_cursor(Some(&"x".repeat(MAX_CURSOR_BYTES + 1))).is_err());
        assert!(validate_uri(&"x".repeat(MAX_URI_BYTES + 1)).is_err());
        assert!(validate_content_text(&"x".repeat(MAX_CONTENT_BYTES + 1)).is_err());
        let mut value = Value::Null;
        for _ in 0..=MAX_JSON_DEPTH {
            value = Value::Array(vec![value]);
        }
        assert_eq!(validate_json(&value), Err(ValidationError::TooDeep));
    }
}
