//! Content-Security-Policy computation and middleware (M24b).

use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request},
    middleware::Next,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use sha2::{Digest, Sha256};

use crate::AppState;

/// SHA-256 content hashes (as `'sha256-…'` CSP source expressions) for the
/// built-in UI's inline scripts and styles. Custom UI packages get their own
/// per-asset hashes; the built-in Trunk bootstrap script and the recovery
/// page style are hashed here so the strict CSP (no `unsafe-inline`) still
/// allows them.
#[derive(Debug, Clone, Default)]
pub struct BuiltinCspHashes {
    pub script_hashes: Vec<String>,
    pub style_hashes: Vec<String>,
}

/// Extract the raw text content of every `<tag …>…</tag>` element in `html`
/// and return CSP `'sha256-…'` source expressions. Content is hashed verbatim
/// (no trimming) to match the CSP3 text-content hash.
pub fn extract_inline_hashes(html: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = html[pos..].find(&open) {
        let open_start = pos + rel;
        let Some(gt_rel) = html[open_start..].find('>') else {
            break;
        };
        let open_tag = &html[open_start..open_start + gt_rel];
        if open_tag.contains("src=") {
            // External resource (e.g. <script src="…">): no inline content.
            pos = open_start + gt_rel + 1;
            continue;
        }
        let content_start = open_start + gt_rel + 1;
        let after = &html[content_start..];
        let Some(close_rel) = after.find(&close) else {
            break;
        };
        let content = &after[..close_rel];
        let digest = Sha256::digest(content.as_bytes());
        out.push(format!("'sha256-{}'", BASE64_STANDARD.encode(digest)));
        pos = content_start + close_rel + close.len();
    }
    out
}

/// Build CSP header value from active UI state and settings.
pub fn build_csp_header(state: &AppState, active: Option<&crate::ActiveUiState>) -> String {
    let connect_src = state
        .settings
        .http
        .public_origin
        .as_str()
        .trim_end_matches('/');
    let builtin = &state.builtin_csp_hashes;
    if let Some(active) = active {
        let mut script_hashes = Vec::new();
        let mut style_hashes = Vec::new();
        for asset in &active.assets {
            if asset.content_type.contains("javascript") || asset.content_type.contains("wasm") {
                script_hashes.push(format!("'sha256-{}'", asset.sha256_hash));
            }
            if asset.content_type.contains("css") {
                style_hashes.push(format!("'sha256-{}'", asset.sha256_hash));
            }
        }
        // The built-in recovery page and (when no custom entry is active) the
        // built-in bootstrap remain reachable; keep their inline hashes
        // allowed even when a custom UI package is active.
        script_hashes.extend(builtin.script_hashes.iter().cloned());
        style_hashes.extend(builtin.style_hashes.iter().cloned());
        let script_src = if script_hashes.is_empty() {
            "'self'".to_string()
        } else {
            script_hashes.join(" ")
        };
        let style_src = if style_hashes.is_empty() {
            "'self'".to_string()
        } else {
            format!("'self' {}", style_hashes.join(" "))
        };
        format!(
            "default-src 'none'; script-src {script_src}; style-src {style_src}; img-src 'self' data:; connect-src {connect_src}; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
        )
    } else {
        let mut script_src = vec!["'self'".to_string(), "'wasm-unsafe-eval'".to_string()];
        script_src.extend(builtin.script_hashes.iter().cloned());
        let mut style_src = vec!["'self'".to_string()];
        style_src.extend(builtin.style_hashes.iter().cloned());
        format!(
            "default-src 'self'; script-src {}; style-src {}; img-src 'self' data:; connect-src {connect_src}; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'",
            script_src.join(" "),
            style_src.join(" ")
        )
    }
}

pub fn build_csp_header_sync(state: &AppState) -> String {
    let active = state.active_ui.try_read().ok().and_then(|g| g.clone());
    build_csp_header(state, active.as_ref())
}

/// Middleware that injects CSP header on every response (sync try_read).
pub async fn csp_middleware(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let guard = state.active_ui.try_read().ok().and_then(|g| g.clone());
    let header_val = build_csp_header(&state, guard.as_ref());
    let mut response = next.run(request).await;
    if let Ok(val) = HeaderValue::from_str(&header_val) {
        response
            .headers_mut()
            .insert("content-security-policy", val);
    }
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// Async variant that correctly awaits active state (used as layer with state clone).
pub async fn csp_middleware_async(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let active = state.active_ui.read().await.clone();
    let header_val = build_csp_header(&state, active.as_ref());
    let mut response = next.run(request).await;
    if let Ok(val) = HeaderValue::from_str(&header_val) {
        response
            .headers_mut()
            .insert("content-security-policy", val);
    }
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_inline_hashes_hashes_verbatim_content() {
        let html = r#"<html><head><style>body{color:red}</style></head><body><script type="module">import init from '/app.js'; init();</script></body></html>"#;
        let scripts = extract_inline_hashes(html, "script");
        let styles = extract_inline_hashes(html, "style");
        assert_eq!(scripts.len(), 1);
        assert_eq!(styles.len(), 1);
        // Independent re-computation of the CSP3 text-content hash.
        let script_digest = Sha256::digest(b"import init from '/app.js'; init();");
        let style_digest = Sha256::digest(b"body{color:red}");
        assert_eq!(
            scripts[0],
            format!("'sha256-{}'", BASE64_STANDARD.encode(script_digest))
        );
        assert_eq!(
            styles[0],
            format!("'sha256-{}'", BASE64_STANDARD.encode(style_digest))
        );
    }

    #[test]
    fn extract_inline_hashes_skips_external_scripts() {
        let html = r#"<script src="/app.js"></script><script>inline()</script>"#;
        let hashes = extract_inline_hashes(html, "script");
        // Only the inline script contributes a content hash.
        assert_eq!(hashes.len(), 1);
        let digest = Sha256::digest(b"inline()");
        assert_eq!(
            hashes[0],
            format!("'sha256-{}'", BASE64_STANDARD.encode(digest))
        );
    }

    #[test]
    fn csp_builder_avoids_unsafe_inline() {
        let fake_public_origin = "https://example.com";
        let header = format!(
            "default-src 'none'; script-src 'sha256-abc123'; style-src 'self' 'sha256-def456'; img-src 'self' data:; connect-src {fake_public_origin}; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
        );
        assert!(!header.contains("unsafe-inline"));
        assert!(!header.contains("unsafe-eval"));
        assert!(header.contains("sha256-abc123"));
        assert!(header.contains(fake_public_origin));
    }

    #[test]
    fn csp_default_allows_wasm_unsafe_eval_only() {
        let header = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src https://example.com; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'";
        assert!(header.contains("wasm-unsafe-eval"));
        assert!(!header.contains("unsafe-inline"));
        let bare_unsafe = header.replacen("wasm-unsafe-eval", "", 1);
        assert!(!bare_unsafe.contains("unsafe-eval"));
    }
}
