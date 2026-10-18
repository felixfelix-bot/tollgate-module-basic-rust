//! GET /whoami — HTTP-02.
//!
//! Returns the device-identifier the customer MUST include in its payment event.
//! A3: resolved from the connection's address via the DHCP lease table then ARP,
//! never from a caller-supplied claim (a claim is client-controlled, so honouring
//! it would let one device impersonate another's paid session).

use crate::http::mac;
use crate::http::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

pub async fn handle_whoami(
    State(_state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let real = headers.get("x-real-ip").and_then(|v| v.to_str().ok());

    let ip = mac::client_ip_from_headers(forwarded, real, None);

    let body = match ip.as_deref().map(mac::resolve_client_mac_default) {
        Some(Some(resolved)) => format!("mac={}", resolved.mac),
        Some(None) => {
            // No identity resolvable: say so rather than inventing one.
            tracing::warn!(ip = ?ip, "whoami: no MAC in leases or ARP");
            "mac=".to_string()
        }
        None => "mac=".to_string(),
    };

    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        body,
    )
}
