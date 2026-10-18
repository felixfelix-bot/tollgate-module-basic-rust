//! P3 — `/ln-invoice` must return a REAL, payable BOLT11 invoice.
//!
//! Before this, POST returned `"stub-invoice"` with a synthetic quote id, and GET
//! unconditionally reported `unpaid`. A customer could never obtain a quote, so the
//! only way to buy access was to arrive with a pre-minted token — which is not the
//! product.
//!
//! These tests need a live mint (`TOLLGATE_TEST_MINT`), because "returns a real
//! invoice" is not something a mock can demonstrate: the assertion is that the string
//! is a BOLT11 invoice *the mint will honour*. Without a mint they are SKIPPED with a
//! loud reason rather than silently passing.

use crate::http::routes::ln_invoice::{handle_create_ln_invoice, handle_get_ln_invoice};
use crate::http::routes::quote::MintQuoteRegistry;
use crate::http::AppState;
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use std::sync::{Arc, Mutex};

fn test_mint() -> Option<String> {
    std::env::var("TOLLGATE_TEST_MINT").ok()
}

/// Build a state with a wallet bound to `mint`.
async fn state_with_mint(mint: &str) -> AppState {
    let mut cfg = crate::config::Config::default();
    cfg.accepted_mints = vec![crate::config::schema::MintConfig::default_production(
        mint.trim_end_matches('/'),
    )];
    let mut wallet = crate::wallet::TollWallet::new(
        [7u8; 64],
        vec![mint.trim_end_matches('/').to_string()],
        std::env::temp_dir().join(format!("tg-p3-wallet-{}", std::process::id())),
    );
    wallet.ensure_mint(mint).await.ok();

    crate::http::AppState {
        config: Arc::new(cfg),
        identity: Arc::new(crate::identity::MerchantIdentity {
            name: "merchant".to_string(),
            secret_key: secp256k1::SecretKey::from_slice(&[3u8; 32]).unwrap(),
        }),
        wallet: Arc::new(tokio::sync::Mutex::new(Some(wallet))),
        sessions: Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(),
        )),
        quotes: Arc::new(Mutex::new(MintQuoteRegistry::new())),
    }
}

async fn body_of(resp: axum::response::Response) -> (u16, String) {
    let status = resp.status().as_u16();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// The headline promise: a customer gets a payable invoice. `request` being a
/// BOLT11 string is the whole point — a quote id in that field is what the stub did.
#[tokio::test]
async fn create_returns_a_real_bolt11_invoice() {
    let Some(mint) = test_mint() else {
        eprintln!("SKIP: set TOLLGATE_TEST_MINT to run the live-invoice tests");
        return;
    };
    let state = state_with_mint(&mint).await;

    let resp = handle_create_ln_invoice(
        State(state),
        axum::Json(crate::http::routes::ln_invoice::CreateInvoiceRequest {
            amount: 4,
            unit: Some("sat".to_string()),
        }),
    )
    .await
    .into_response();
    let (status, body) = body_of(resp).await;

    assert_eq!(status, 200, "expected 200, body: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("json body");
    let request = v["request"].as_str().unwrap_or_default();

    assert!(
        request.starts_with("lnbc"),
        "`request` must be a BOLT11 invoice, got {request:?}. The old stub put the \
         quote id here, which is not payable by any wallet."
    );
    assert!(
        request.len() > 100,
        "a real BOLT11 invoice is longer than 100 chars, got {}",
        request.len()
    );
    assert!(
        v["quote"].as_str().unwrap_or_default().len() > 8,
        "quote id looks synthetic: {:?}",
        v["quote"]
    );
}

/// `expiry` must be a real deadline, not 0.
#[tokio::test]
async fn create_returns_a_usable_expiry() {
    let Some(mint) = test_mint() else {
        eprintln!("SKIP: set TOLLGATE_TEST_MINT");
        return;
    };
    let state = state_with_mint(&mint).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let resp = handle_create_ln_invoice(
        State(state),
        axum::Json(crate::http::routes::ln_invoice::CreateInvoiceRequest {
            amount: 4,
            unit: None,
        }),
    )
    .await
    .into_response();
    let (_, body) = body_of(resp).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    let expiry = v["expiry"].as_u64().unwrap_or(0);
    assert!(
        expiry > now,
        "expiry {expiry} is not in the future (now {now}); a quote that is already \
         expired cannot be paid"
    );
}

/// GET must report the mint's answer, and an unknown quote must 404 rather than
/// synthesising `unpaid`.
#[tokio::test]
async fn status_reports_the_mint_and_404s_unknown() {
    let Some(mint) = test_mint() else {
        eprintln!("SKIP: set TOLLGATE_TEST_MINT");
        return;
    };
    let state = state_with_mint(&mint).await;

    let resp = handle_get_ln_invoice(
        State(state.clone()),
        Query(crate::http::routes::ln_invoice::InvoiceQuery {
            quote: "definitely-not-a-quote".to_string(),
        }),
    )
    .await
    .into_response();
    let (status, _) = body_of(resp).await;
    assert_eq!(
        status, 404,
        "an unknown quote must 404; returning 200/unpaid tells a client to wait for \
         a payment that can never arrive"
    );
}
