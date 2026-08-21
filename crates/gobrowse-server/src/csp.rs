//! Content-Security-Policy computation and middleware (M24b).

use axum::{
    body::Body,
    extract::State,
    http::{HeaderValue, Request},
    middleware::Next,
    response::Response,
};

use crate::AppState;

/// Build CSP header value from active UI state and settings.
pub fn build_csp_header(state: &AppState, active: Option<&crate::ActiveUiState>) -> String {
    let connect_src = state
        .settings
        .http
        .public_origin
        .as_str()
        .trim_end_matches('/');
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
        format!(
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data:; connect-src {connect_src}; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"
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
