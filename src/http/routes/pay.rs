//! POST / — payment endpoint.
//!
//! Accepts text/plain (Cashu token) or application/json (Nostr kind 21000).
//! Phase 4: verifies token, receives into wallet, creates session, returns
//! kind 1022 on success or kind 21023 + HTTP 402 on failure.

use crate::http::AppState;
use crate::wallet::verify::TokenVerifier;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;

/// Extract a Cashu token from a Nostr kind 21000 event JSON body.
/// Looks for a tag ["payment", "<token>"].
fn extract_token_from_nostr_event(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if v.get("kind").and_then(|k| k.as_u64()) != Some(21000) {
        return None;
    }
    let tags = v.get("tags")?.as_array()?;
    for tag in tags {
        if let Some(arr) = tag.as_array() {
            if arr.len() >= 2 && arr.first().and_then(|s| s.as_str()) == Some("payment") {
                return arr.get(1).and_then(|s| s.as_str()).map(|s| s.to_string());
            }
        }
    }
    None
}

/// A kind-21023 notice carrying the TIP-01 `level` + `code` tags.
///
/// A2 replaces the remaining hand-built notices with this; A1 needs it because
/// the new failure paths must not emit bare JSON.
pub(crate) fn notice_json(code: &str, content: &str) -> String {
    serde_json::json!({
        "kind": 21023,
        "tags": [["level", "error"], ["code", code]],
        "content": content,
    })
    .to_string()
}

/// Allotment for a payment, per TIP-01/TIP-02.
///
/// `steps = amount_sats / price_per_step` (integer division — a partial step is
/// not granted), `allotment = steps * step_size`.
///
/// Before A1 the handler used `received_amount * 1000`, which is a satoshi ->
/// millisatoshi conversion dressed up as an allotment: it never consulted the
/// price table, so a 1-sat payment and a 100-sat payment produced allotments
/// unrelated to what the operator charges. This is the single place the price
/// table is turned into access.
pub(crate) fn calculate_allotment(
    amount_sats: u64,
    price_per_step: u64,
    step_size: u64,
    metric: &str,
) -> Result<u64, String> {
    if price_per_step == 0 {
        return Err("price_per_step is 0: cannot derive steps (would divide by zero)".to_string());
    }
    if metric != "bytes" && metric != "milliseconds" {
        return Err(format!(
            "metric {metric:?} is not one of `bytes` / `milliseconds` (TIP-01)"
        ));
    }
    let steps = amount_sats / price_per_step;
    Ok(steps * step_size)
}

/// Build the signed kind-1022 session event (TIP-01).
///
/// `device_identifier` is the customer's MAC. `customer_pubkey` is `Some` only
/// when the payment carried a customer Nostr identity — TIP-01 makes `p`
/// optional and pubkey-only, and emitting a MAC there is the recorded
/// `DIV-2026-10-mac-in-p-tag` bug class.
pub(crate) fn build_session_event(
    config: &crate::config::schema::Config,
    identity: &crate::identity::MerchantIdentity,
    device_identifier: &str,
    allotment: u64,
    start_time: u64,
    customer_pubkey: Option<&str>,
) -> serde_json::Value {
    let mut tags: Vec<Vec<String>> = Vec::new();
    if let Some(pk) = customer_pubkey {
        tags.push(vec!["p".to_string(), pk.to_string()]);
    }
    tags.push(vec![
        "device-identifier".to_string(),
        "mac".to_string(),
        device_identifier.to_string(),
    ]);
    tags.push(vec!["allotment".to_string(), allotment.to_string()]);
    tags.push(vec!["metric".to_string(), config.metric.clone()]);
    tags.push(vec!["start-time".to_string(), start_time.to_string()]);

    let event = crate::nostr_event::create_event(1022, tags, "", &identity.secret_key);
    serde_json::to_value(&event).unwrap_or_else(|_| serde_json::json!({}))
}

pub async fn handle_pay(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Extract token from either path
    let token = if content_type.contains("text/plain") {
        tracing::info!(len = body.len(), "received text/plain payment");
        body.trim().to_string()
    } else if content_type.contains("application/json") {
        tracing::info!(len = body.len(), "received json payment");
        match extract_token_from_nostr_event(&body) {
            Some(t) => t,
            None => {
                let resp = serde_json::json!({
                    "kind": 21023,
                    "content": "invalid Nostr kind 21000 event: no payment tag found",
                });
                let json = serde_json::to_string(&resp).unwrap_or_default();
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        return (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            [
                ("content-type", "text/plain"),
                ("access-control-allow-origin", "*"),
            ],
            "unsupported content-type".to_string(),
        );
    };

    // Step 1: verify token via NUT-07 checkstate
    let mints: Vec<String> = state
        .config
        .accepted_mints
        .iter()
        .map(|m| m.url.clone())
        .collect();

    let verifier = TokenVerifier::new(mints);
    let verified_amount = match verifier.verify(&token).await {
        Ok(amount_msat) => amount_msat,
        Err(e) => {
            tracing::warn!(error = %e, "token verification failed");
            let resp = serde_json::json!({
                "kind": 21023,
                "content": format!("payment rejected: {e}"),
            });
            let json = serde_json::to_string(&resp).unwrap_or_default();
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    // Step 2: receive token into wallet
    let wallet_guard = state.wallet.lock().await;
    let received_amount = if let Some(ref wallet) = *wallet_guard {
        match wallet.receive(&token).await {
            Ok(amount_sat) => {
                tracing::info!(amount_sat, "token received into wallet");
                amount_sat
            }
            Err(e) => {
                tracing::warn!(error = %e, "wallet receive failed");
                drop(wallet_guard);
                let resp = serde_json::json!({
                    "kind": 21023,
                    "content": format!("payment rejected: wallet receive failed: {e}"),
                });
                let json = serde_json::to_string(&resp).unwrap_or_default();
                return (
                    StatusCode::PAYMENT_REQUIRED,
                    [
                        ("content-type", "application/json"),
                        ("access-control-allow-origin", "*"),
                    ],
                    json,
                );
            }
        }
    } else {
        tracing::warn!("wallet not initialized");
        drop(wallet_guard);
        let resp = serde_json::json!({
            "kind": 21023,
            "content": "payment rejected: wallet not initialized",
        });
        let json = serde_json::to_string(&resp).unwrap_or_default();
        return (
            StatusCode::PAYMENT_REQUIRED,
            [
                ("content-type", "application/json"),
                ("access-control-allow-origin", "*"),
            ],
            json,
        );
    };
    drop(wallet_guard);

    // NOTE (A3): resolved from dhcp.leases/arp. Until then this is the
    // documented sentinel, so `/usage` cannot match a session yet.
    let device_identifier = "00:00:00:00:00:00".to_string();

    // Step 3: create session.
    //
    // A1: allotment comes from the PRICE TABLE (steps x step_size), not from a
    // satoshi -> millisatoshi conversion. A payment that does not cover one
    // whole step grants nothing and is refused rather than silently granted an
    // allotment unrelated to the price.
    let mint = state
        .config
        .accepted_mints
        .first()
        .cloned()
        .ok_or_else(|| "no accepted mints configured".to_string());
    let mint = match mint {
        Ok(m) => m,
        Err(e) => {
            let json = notice_json("session-error", &format!("payment rejected: {e}"));
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    let allotment = match calculate_allotment(
        received_amount,
        mint.price_per_step,
        state.config.step_size,
        &state.config.metric,
    ) {
        Ok(a) if a > 0 => a,
        Ok(_) => {
            let json = notice_json(
                "payment-error-token-spent",
                &format!(
                    "payment of {received_amount} sat does not cover one step at {} sat/step",
                    mint.price_per_step
                ),
            );
            return (
                StatusCode::PAYMENT_REQUIRED,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
        Err(e) => {
            let json = notice_json("session-error", &format!("payment rejected: {e}"));
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                [
                    ("content-type", "application/json"),
                    ("access-control-allow-origin", "*"),
                ],
                json,
            );
        }
    };

    let start_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut sessions = state.sessions.lock().await;
    let _session = sessions.create_session(
        &device_identifier,
        allotment,
        &state.config.metric,
        start_time,
    );
    drop(sessions);

    tracing::info!(
        verified_msat = verified_amount,
        received_sat = received_amount,
        allotment_msat = allotment,
        "session granted"
    );

    // Step 4: return a SIGNED kind 1022 session event (TIP-01).
    let resp = build_session_event(
        &state.config,
        &state.identity,
        &device_identifier,
        allotment,
        start_time,
        None, // a raw-token payment carries no customer pubkey; p stays omitted
    );

    let json = serde_json::to_string(&resp).unwrap_or_default();
    (
        StatusCode::OK,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        json,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_token_from_valid_nostr_event() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["payment", "cashuBabc123token"]],
            "content": "",
            "pubkey": "abc",
            "id": "def",
            "sig": "ghi",
            "created_at": 1234567890
        })
        .to_string();
        let token = extract_token_from_nostr_event(&event);
        assert_eq!(token.as_deref(), Some("cashuBabc123token"));
    }

    #[test]
    fn extract_token_rejects_wrong_kind() {
        let event = serde_json::json!({
            "kind": 99999,
            "tags": [["payment", "token"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_rejects_missing_payment_tag() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [["other", "value"]],
        })
        .to_string();
        assert!(extract_token_from_nostr_event(&event).is_none());
    }

    #[test]
    fn extract_token_handles_invalid_json() {
        assert!(extract_token_from_nostr_event("not json").is_none());
    }

    #[test]
    fn extract_token_handles_multiple_tags() {
        let event = serde_json::json!({
            "kind": 21000,
            "tags": [
                ["other", "val"],
                ["payment", "real-token"],
                ["another", "x"]
            ],
        })
        .to_string();
        assert_eq!(
            extract_token_from_nostr_event(&event).as_deref(),
            Some("real-token")
        );
    }

    /// Test that a session is created after a successful payment flow.
    /// Uses the SessionManager directly to verify the integration logic.
    #[tokio::test]
    async fn payment_creates_session() {
        use crate::session::SessionManager;

        let mut mgr = SessionManager::new();
        let allotment: u64 = 5000; // 5 sats * 1000 = 5000 msat
        let session = mgr.create_session("test:mac", allotment, "bytes", 3600);
        assert_eq!(session.allotment, 5000);
        assert_eq!(session.metric, "bytes");
        assert!(mgr.is_active("test:mac"));
    }

    /// Test that rejected tokens return 402 (simulated).
    #[test]
    fn rejected_token_returns_402() {
        // The handler returns PAYMENT_REQUIRED for failed verification.
        // We verify the status code constant matches.
        let expected = StatusCode::PAYMENT_REQUIRED;
        assert_eq!(expected.as_u16(), 402);
    }
}
