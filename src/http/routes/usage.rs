//! GET /usage — returns plain text "used/total" or "-1/-1".
//!
//! A3: the caller's address comes from X-Forwarded-For / X-Real-Ip / the socket,
//! and is then resolved to a MAC through the DHCP lease table and ARP — the same
//! identity the payment handler keyed the session on. Keying on the address
//! itself (the pre-A3 behaviour) could never match a session.

use crate::http::mac;
use crate::http::AppState;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

pub async fn handle_usage(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    // A3: the session is keyed on the resolved MAC, so this must resolve the
    // SAME identity the payment handler used. Keying on X-Forwarded-For (the
    // pre-A3 behaviour) could never match a session created under a MAC.
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let real = headers.get("x-real-ip").and_then(|v| v.to_str().ok());

    let mac = mac::client_ip_from_headers(forwarded, real, None)
        .as_deref()
        .and_then(mac::resolve_client_mac_default)
        .map(|r| r.mac);

    let Some(mac) = mac else {
        return usage_response("-1/-1");
    };

    let sessions = state.sessions.lock().await;
    match sessions.get_session(&mac) {
        Some(session) if sessions.is_active(&mac) => {
            let used = session.used;
            let total = session.allotment;
            drop(sessions);
            usage_response(format!("{used}/{total}"))
        }
        // exists but expired or exhausted, or never existed
        _ => {
            drop(sessions);
            usage_response("-1/-1")
        }
    }
}

fn usage_response(
    body: impl Into<String>,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        StatusCode::OK,
        [
            ("content-type", "text/plain"),
            ("access-control-allow-origin", "*"),
        ],
        body.into(),
    )
}

#[cfg(test)]
mod tests {
    use crate::session::SessionManager;
    use std::sync::Arc;

    #[tokio::test]
    async fn usage_returns_neg1_without_session() {
        let mgr = Arc::new(tokio::sync::Mutex::new(SessionManager::new()));
        let session = mgr.lock().await;
        assert!(session.get_session("any").is_none());
        drop(session);
        // Without a session, the response would be "-1/-1"
        let expected = "-1/-1";
        assert_eq!(expected, "-1/-1");
    }

    #[tokio::test]
    async fn usage_returns_used_total_with_session() {
        let mut mgr = SessionManager::new();
        mgr.create_session("192.168.1.100", 5000, "bytes", 3600);
        {
            let s = mgr.sessions.get_mut("192.168.1.100").unwrap();
            s.used = 1500;
        }
        let session = mgr.get_session("192.168.1.100").unwrap();
        assert!(mgr.is_active("192.168.1.100"));
        let expected = format!("{}/{}", session.used, session.allotment);
        assert_eq!(expected, "1500/5000");
    }

    #[tokio::test]
    async fn usage_returns_neg1_for_expired_session() {
        let mut mgr = SessionManager::new();
        mgr.create_session("192.168.1.100", 5000, "bytes", 0);
        // Force expiry
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        {
            let s = mgr.sessions.get_mut("192.168.1.100").unwrap();
            s.expiry = now - 1;
        }
        assert!(!mgr.is_active("192.168.1.100"));
    }
}
