//! GET /usage — returns plain text "used/total" or "-1/-1".

use axum::extract::State;
use axum::response::IntoResponse;
use axum::http::StatusCode;
use crate::http::AppState;

pub async fn handle_usage(
    State(_state): State<AppState>,
) -> impl IntoResponse {
    // Phase 1: no session state. Phase 4 wires to ndsctl.
    (StatusCode::OK, [("content-type", "text/plain"), ("access-control-allow-origin", "*")], "-1/-1")
}
