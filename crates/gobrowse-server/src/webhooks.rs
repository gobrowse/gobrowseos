//! Inbound webhook receive, verify, and dedup.
//!
//! Webhooks are delivered as `POST /api/v1/webhooks/{id}/deliver` with
//! headers carrying a delivery id, timestamp, and HMAC signature. The
//! signing payload is `{timestamp_unix_ms}.{raw_body_bytes}`.
//!
//! HMAC-SHA256 is implemented manually (RFC 2104) using the `sha2` crate
//! because the `hmac` crate is not a workspace dependency and pinned-deps
//! policy forbids adding new crates.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use sha2::{Digest, Sha256};
use sqlx::Row;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::{AppState, error::AppError};

/// Maximum allowed clock skew between the webhook sender's timestamp
/// and the server's clock, in seconds.
const MAX_CLOCK_SKEW_SECS: i64 = 300;

/// HMAC-SHA256 block size in bytes (RFC 2104 §2).
const SHA256_BLOCK_SIZE: usize = 64;

/// Inner pad byte (RFC 2104 §2).
const IPAD: u8 = 0x36;

/// Outer pad byte (RFC 2104 §2).
const OPAD: u8 = 0x5c;

/// Compute HMAC-SHA256(key, message) per RFC 2104 using only `sha2::Sha256`.
///
/// RFC 2104 §2 algorithm:
/// 1. If key > blocksize, hash the key (key = H(key))
/// 2. Pad key to blocksize with zeros
/// 3. o_key_pad = key XOR [0x5c repeated blocksize times]
/// 4. i_key_pad = key XOR [0x36 repeated blocksize times]
/// 5. HMAC = H(o_key_pad || H(i_key_pad || message))
///
/// # Safety
/// Pure safe Rust. No `unsafe` code.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // Step 1-2: normalize key to exactly blocksize bytes
    let mut key_block = [0u8; SHA256_BLOCK_SIZE];

    if key.len() > SHA256_BLOCK_SIZE {
        // Key longer than block: hash it first
        let mut h = Sha256::new();
        h.update(key);
        let hashed = h.finalize();
        key_block[..32].copy_from_slice(&hashed);
    } else {
        // Key ≤ block size: use as-is, zero-padded
        key_block[..key.len()].copy_from_slice(key);
    }

    // Step 3: inner hash = H((key XOR ipad) || message)
    let mut ipad_block = [IPAD; SHA256_BLOCK_SIZE];
    for i in 0..SHA256_BLOCK_SIZE {
        ipad_block[i] ^= key_block[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad_block);
    inner.update(message);
    let inner_hash = inner.finalize();

    // Step 4: outer hash = H((key XOR opad) || inner_hash)
    let mut opad_block = [OPAD; SHA256_BLOCK_SIZE];
    for i in 0..SHA256_BLOCK_SIZE {
        opad_block[i] ^= key_block[i];
    }
    let mut outer = Sha256::new();
    outer.update(opad_block);
    outer.update(inner_hash);
    outer.finalize().into()
}

/// Verify an HMAC-SHA256 signature with the pre-built payload bytes.
///
/// `secret` is the raw HMAC secret bytes.
/// `payload` is the signing payload (timestamp || "." || body).
/// `signatures` are the values from the `X-Gobrowse-Signature` header,
/// each optionally prefixed with `sha256=`.
///
/// Returns `true` if any provided signature matches using constant-time comparison.
pub fn verify_signature_with_payload(secret: &[u8], payload: &[u8], signatures: &[String]) -> bool {
    let expected = hmac_sha256(secret, payload);
    let expected_hex = hex_encode(&expected);

    signatures.iter().any(|sig| {
        let provided = sig.strip_prefix("sha256=").unwrap_or(sig);
        expected_hex.as_bytes().ct_eq(provided.as_bytes()).into()
    })
}

/// Lowercase hex encoding for a byte slice.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// POST /api/v1/webhooks/{id}/deliver
///
/// Inbound webhook delivery. This is an unauthenticated endpoint — the
/// webhook ID and HMAC signature provide authentication.
///
/// Required headers:
/// - `X-Gobrowse-Signature`: `sha256=<hex>` (comma-separated if multiple)
/// - `X-Gobrowse-Delivery`: `<uuid>` (idempotency key)
/// - `X-Gobrowse-Timestamp`: `<unix millis>` (replay/time-skew protection)
///
/// Status codes:
/// - 200: delivery accepted
/// - 400: malformed headers
/// - 401: signature verification failed or stale timestamp
/// - 404: webhook not found or disabled
/// - 409: delivery already processed (replay)
pub async fn receive_webhook(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<impl IntoResponse, AppError> {
    // --- Extract and validate headers ---
    let signature_header = headers
        .get("x-gobrowse-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Validation("missing X-Gobrowse-Signature header".into()))?;
    let signatures: Vec<String> = signature_header
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let delivery_id_str = headers
        .get("x-gobrowse-delivery")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Validation("missing X-Gobrowse-Delivery header".into()))?;
    let delivery_id: Uuid = Uuid::parse_str(delivery_id_str.trim())
        .map_err(|_| AppError::Validation("invalid X-Gobrowse-Delivery UUID".into()))?;

    let timestamp_str = headers
        .get("x-gobrowse-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Validation("missing X-Gobrowse-Timestamp header".into()))?;
    let timestamp_millis: i64 = timestamp_str
        .trim()
        .parse()
        .map_err(|_| AppError::Validation("invalid X-Gobrowse-Timestamp".into()))?;

    // --- Load webhook ---
    let row = sqlx::query("SELECT enabled, secret_key FROM webhooks WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)?;

    let enabled: bool = row.get("enabled");
    if !enabled {
        return Err(AppError::NotFound);
    }

    let secret_key: Vec<u8> = row
        .get::<Option<Vec<u8>>, _>("secret_key")
        .ok_or_else(|| {
            AppError::Validation(
                "webhook has no direct secret_key; vault-backed secrets not yet supported for inbound delivery"
                    .into(),
            )
        })?;

    // --- Clock skew check ---
    let now = time::OffsetDateTime::now_utc();
    let now_millis = now.unix_timestamp() * 1000 + now.millisecond() as i64;
    let skew_secs = (now_millis - timestamp_millis).abs() / 1000;
    if skew_secs > MAX_CLOCK_SKEW_SECS {
        return Err(AppError::Unauthorized);
    }

    // --- Signature verification ---
    let payload = format!("{timestamp_millis}.{}", String::from_utf8_lossy(&body));
    if !verify_signature_with_payload(&secret_key, payload.as_bytes(), &signatures) {
        return Err(AppError::Unauthorized);
    }

    // --- Idempotent delivery insert ---
    let result = sqlx::query(
        "INSERT INTO webhook_deliveries (webhook_id, delivery_id) \
         VALUES ($1, $2) ON CONFLICT (webhook_id, delivery_id) DO NOTHING",
    )
    .bind(id)
    .bind(delivery_id.to_string())
    .execute(&state.pool)
    .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::Conflict("delivery already processed"));
    }

    Ok((StatusCode::OK, "delivery accepted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simple hex decoder for test vectors only (no `hex` crate dependency).
    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn hmac_sha256_rfc_test_vector_1_short_key() {
        // RFC 4231 test case 1: key = 20 bytes of 0x0b
        let key = [0x0b_u8; 20];
        let data = b"Hi There";
        let expected =
            hex_decode("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        let result = hmac_sha256(&key, data);
        assert_eq!(result.to_vec(), expected);
    }

    #[test]
    fn hmac_sha256_rfc_test_vector_2_string_key() {
        // RFC 4231 test case 2: key = "Jefe"
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let expected =
            hex_decode("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
        let result = hmac_sha256(key, data);
        assert_eq!(result.to_vec(), expected);
    }

    #[test]
    fn hmac_sha256_rfc_test_vector_3_long_key() {
        // RFC 4231 test case 6 (abridged): key = 131 bytes of 0xaa
        let key = [0xaa_u8; 131];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        let expected =
            hex_decode("60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54");
        let result = hmac_sha256(&key, data);
        assert_eq!(result.to_vec(), expected);
    }

    #[test]
    fn verify_signature_accepts_matching_signature() {
        let secret = b"super-secret-key";
        let body = b"hello world";
        let ts: i64 = 1_700_000_000_000;
        let payload = format!("{ts}.{}", String::from_utf8_lossy(body));

        let mac = hmac_sha256(secret, payload.as_bytes());
        let hex_mac = hex_encode(&mac);
        let sig_header = format!("sha256={hex_mac}");

        assert!(verify_signature_with_payload(
            secret,
            payload.as_bytes(),
            &[sig_header]
        ));
    }

    #[test]
    fn verify_signature_rejects_tampered_body() {
        let secret = b"super-secret-key";
        let body = b"hello world";
        let ts: i64 = 1_700_000_000_000;
        let payload = format!("{ts}.{}", String::from_utf8_lossy(body));

        let mac = hmac_sha256(secret, payload.as_bytes());
        let hex_mac = hex_encode(&mac);

        // Different payload with same signature must not match
        let tampered = format!("{ts}.{}", String::from_utf8_lossy(b"hacked!"));
        let sig_header = format!("sha256={hex_mac}");

        assert!(!verify_signature_with_payload(
            secret,
            tampered.as_bytes(),
            &[sig_header]
        ));
    }

    #[test]
    fn verify_signature_accepts_any_of_multiple_signatures() {
        let secret = b"super-secret-key";
        let body = b"test";
        let ts: i64 = 1_700_000_000_000;
        let payload = format!("{ts}.{}", String::from_utf8_lossy(body));

        let mac = hmac_sha256(secret, payload.as_bytes());
        let hex_mac = hex_encode(&mac);

        let sigs = vec![
            "sha256=deadbeef00000000000000000000000000000000000000000000000000000000".into(),
            format!("sha256={hex_mac}"),
        ];

        assert!(verify_signature_with_payload(
            secret,
            payload.as_bytes(),
            &sigs
        ));
    }

    #[test]
    fn verify_signature_constant_time_rejects_wrong_length() {
        let secret = b"key";
        let payload = b"data";
        let sig = "sha256=too-short".to_string();

        assert!(!verify_signature_with_payload(secret, payload, &[sig]));
    }

    #[test]
    fn verify_signature_does_not_require_sha256_prefix() {
        let secret = b"my-secret";
        let body = b"some data";
        let ts: i64 = 1_700_000_000_000;
        let payload = format!("{ts}.{}", String::from_utf8_lossy(body));

        let mac = hmac_sha256(secret, payload.as_bytes());
        let hex_mac = hex_encode(&mac);

        // Both prefixed and bare hex should work
        assert!(verify_signature_with_payload(
            secret,
            payload.as_bytes(),
            &[format!("sha256={hex_mac}")]
        ));
        assert!(verify_signature_with_payload(
            secret,
            payload.as_bytes(),
            std::slice::from_ref(&hex_mac)
        ));
    }
}
