//! Shared bounds for untrusted MCP messages and returned content.

use serde_json::Value;

pub const MAX_ITEMS: usize = 256;
pub const MAX_CURSOR_BYTES: usize = 512;
pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_CONTENT_BYTES: usize = 512 * 1024;
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

pub fn validate_content_text(text: &str) -> Result<(), ValidationError> {
    (text.len() <= MAX_CONTENT_BYTES)
        .then_some(())
        .ok_or(ValidationError::ContentTooLarge)
}

pub fn validate_json(value: &Value) -> Result<(), ValidationError> {
    fn walk(value: &Value, depth: usize) -> Result<(), ValidationError> {
        if depth > MAX_JSON_DEPTH {
            return Err(ValidationError::TooDeep);
        }
        match value {
            Value::Array(values) => values.iter().try_for_each(|value| walk(value, depth + 1)),
            Value::Object(values) => values.values().try_for_each(|value| walk(value, depth + 1)),
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
