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

/// Fallback quote lifetime when the mint does not state one. TIP-01 does not fix a
/// lifetime; 15 minutes is long enough to pay an invoice and short enough that a stale
/// quote does not linger.
const DEFAULT_QUOTE_TTL_SECS: u64 = 15 * 60;

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

    // P3: ask the MINT for a quote. Before this the handler invented `quote-<ts>-<rnd>`
    // and left `request` empty, so no client could ever pay: a quote id is not an
    // invoice. The wallet methods already existed (`request_mint_quote`) and were simply
    // never called from here.
    let mint_url = {
        let guard = state.wallet.lock().await;
        match guard.as_ref() {
            Some(w) => match w.default_mint() {
                // Owned: the guard is dropped at the end of this block, so a borrowed
                // &str would not outlive it.
                Some(m) => m.to_string(),
                None => {
                    return error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "no mint is configured, so no invoice can be issued",
                    )
                }
            },
            None => {
                return error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "wallet is not initialised, so no invoice can be issued",
                )
            }
        }
    };

    let quote = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return error_response(StatusCode::SERVICE_UNAVAILABLE, "wallet unavailable");
        };
        match w.request_mint_quote(&mint_url, req.amount).await {
            Ok(q) => q,
            Err(e) => {
                // A mint failure must NOT be dressed up as a quote. Telling a customer
                // "here is your invoice" when there is none is worse than an error.
                tracing::error!(mint = %mint_url, error = %e, "mint quote request failed");
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("the mint did not issue a quote: {e}"),
                );
            }
        }
    };

    // A quote with no invoice is not payable. Refuse rather than return an empty
    // `request`, which is exactly what the previous stub did.
    if quote.request.is_empty() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            "the mint returned a quote without an invoice",
        );
    }

    // The mint states an expiry; fall back only if it says 0, which would mean
    // "already expired" and no client could pay.
    let expiry = if quote.expiry > now {
        quote.expiry
    } else {
        now + DEFAULT_QUOTE_TTL_SECS
    };

    registry(&state)
        .lock()
        .map(|mut r| r.insert(&quote.id, req.amount, expiry))
        .ok();

    let resp = InvoiceResponse {
        quote: quote.id,
        request: quote.request,
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

    // P3: ask the MINT what happened, rather than reporting our own cached guess. A
    // quote we still believe is UNPAID may already be settled; telling the client to
    // wait would be wrong. If the mint is unreachable we fall back to the cached state
    // and SAY SO via `checkState`, rather than fabricating a fresh answer.
    let mut state_str = quote.state.as_str().to_string();
    let mut check_state = match quote.state {
        QuoteState::Unpaid => "UNPAID",
        QuoteState::Paid => "PAID",
        QuoteState::Issued => "ISSUED",
    }
    .to_string();

    {
        let guard = state.wallet.lock().await;
        if let Some(w) = guard.as_ref() {
            if let Some(mint_url) = w.default_mint() {
                match w.check_mint_quote(&mint_url, &quote.id).await {
                    Ok(mint_state) => {
                        // Normalise to the vocabulary this endpoint promises. An
                        // unrecognised mint answer is reported verbatim rather than
                        // coerced to UNPAID -- coercing would tell a paying customer
                        // to pay again.
                        let upper = mint_state.to_uppercase();
                        check_state = if upper.contains("ISSUED") {
                            "ISSUED".to_string()
                        } else if upper.contains("PAID") {
                            "PAID".to_string()
                        } else if upper.contains("UNPAID") {
                            "UNPAID".to_string()
                        } else {
                            mint_state.clone()
                        };
                        state_str = check_state.to_lowercase();
                        if let Ok(mut r) = registry(&state).lock() {
                            match check_state.as_str() {
                                "PAID" => r.mark_paid(&quote.id, ""),
                                "ISSUED" => r.mark_issued(&quote.id),
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(quote = %quote.id, error = %e,
                            "could not reach the mint; reporting cached state");
                        check_state = format!("{check_state}-CACHED");
                    }
                }
            }
        }
    }

    let resp = InvoiceStatus {
        quote: quote.id.clone(),
        state: state_str,
        check_state,
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
