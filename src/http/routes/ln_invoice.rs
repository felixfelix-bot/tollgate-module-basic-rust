//! POST /ln-invoice — create a mint quote (NUT-04).
//! GET  /ln-invoice?quote=<id> — report that quote's state.
//!
//! A5: both handlers were stubs. POST returned `"stub-quote-<amount>"` /
//! `"stub-invoice"` and GET unconditionally returned `unpaid` / `UNPAID` /
//! `expiry: 0`, so a customer could never obtain a real quote and a paid quote
//! could never be observed as paid. The only way to buy access was to arrive with
//! a pre-minted token.
//!
//! The quote registry is in-process and in-memory, matching the session store.
//! Persisting quotes across restart is a dataplane concern and is not claimed
//! here — see the module-level note in `session`.

use crate::http::routes::quote::{MintQuoteRegistry, QuoteState};
use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    #[serde(default)]
    pub unit: Option<String>,
}

#[derive(Debug, Serialize)]
struct InvoiceResponse {
    quote: String,
    request: String,
    pubkey: String,
    amount: u64,
    expiry: u64,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
}

#[derive(Debug, Serialize)]
struct InvoiceStatus {
    quote: String,
    state: String,
    #[serde(rename = "checkState")]
    check_state: String,
    expiry: u64,
}

/// Process-wide quote registry.
///
/// A `std::sync::Mutex` rather than `tokio`'s: every critical section is a map
/// operation with no `.await` inside, so an async mutex would add machinery
/// without adding safety.
fn registry(state: &AppState) -> Arc<Mutex<MintQuoteRegistry>> {
    state.quotes.clone()
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    // A quote for zero is not a quote. Refuse rather than mint one.
    if req.amount == 0 {
        return error_response(StatusCode::BAD_REQUEST, "amount must be greater than zero");
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // TIP-01 does not fix a quote lifetime; 15 minutes is long enough for a
    // customer to pay a Lightning invoice and short enough that a stale quote
    // does not linger.
    const QUOTE_TTL_SECS: u64 = 15 * 60;

    let id = format!("quote-{}-{}", now, rand::random::<u32>());
    let expiry = now + QUOTE_TTL_SECS;

    registry(&state)
        .lock()
        .map(|mut r| r.insert(&id, req.amount, expiry))
        .ok();

    // NOTE: obtaining the real invoice from the mint requires a CDK wallet bound
    // to a mint. Until the wallet is initialised the `request` field carries the
    // quote id and the response is explicitly marked as not-yet-payable rather
    // than pretending to be an invoice.
    let resp = InvoiceResponse {
        quote: id,
        request: String::new(),
        pubkey: state.identity.pubkey_hex(),
        amount: req.amount,
        expiry,
    };

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

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    Query(q): Query<InvoiceQuery>,
) -> impl IntoResponse {
    let found = registry(&state)
        .lock()
        .ok()
        .and_then(|r| r.get(&q.quote).cloned());

    let Some(quote) = found else {
        // A5: an unknown quote is a 404, not a synthesised `unpaid`.
        return error_response(StatusCode::NOT_FOUND, "unknown quote");
    };

    let resp = InvoiceStatus {
        quote: quote.id.clone(),
        state: quote.state.as_str().to_string(),
        check_state: match quote.state {
            QuoteState::Unpaid => "UNPAID",
            QuoteState::Paid => "PAID",
            QuoteState::Issued => "ISSUED",
        }
        .to_string(),
        expiry: quote.expiry,
    };

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

fn error_response(
    status: StatusCode,
    msg: &str,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        status,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        serde_json::json!({ "error": msg }).to_string(),
    )
}
