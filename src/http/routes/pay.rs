//! POST / — payment endpoint (stub for Phase 1).
//!
//! Accepts text/plain (cashu token) or application/json (Nostr kind 21000).
//! Phase 2-3 implements real verification + wallet logic.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::http::{StatusCode, HeaderMap};
use crate::http::AppState;

pub async fn handle_pay(
    State(_state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("text/plain") {
        tracing::info!(len = body.len(), "received text/plain payment (stub)");
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment (stub)");
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [("content-type", "text/plain"), ("access-control-allow-origin", "*")],
            "unsupported content-type",
        );
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        [("content-type", "application/json"), ("access-control-allow-origin", "*")],
        r#"{"error":"payment not yet implemented"}"#,
    )
}
