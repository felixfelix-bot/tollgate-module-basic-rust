//! `POST /ln-invoice` and `GET /ln-invoice?quote=` — HTTP-03.
//!
//! Wire shape comes from `corpus/http/http03-ln-invoice.json`, not from invention. P3
//! shipped `{quote, request, pubkey, amount, expiry}`, which no client expects: the
//! corpus fixes `{status, quote, invoice, mint_url, amount, expiry, state,
//! access_granted}` and uses `status: 1` rather than an HTTP-only success signal.
//!
//! P5 adds the step that made this lane pointless: when a quote is PAID the merchant
//! must MINT the tokens and GRANT the session. Before this, `mint_tokens` was never
//! called from production code and `create_session` only from tests — a customer could
//! pay an invoice and receive nothing at all.

use crate::http::routes::quote::{MintQuoteRegistry, QuoteState};
use crate::http::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Fallback quote lifetime when the mint does not state one.
const DEFAULT_QUOTE_TTL_SECS: u64 = 15 * 60;
/// Steps granted per sat, from the corpus's own `price_per_step = 1` default.
const DEFAULT_METRIC: &str = "bytes";

#[derive(Debug, Deserialize)]
pub struct CreateInvoiceRequest {
    #[serde(default)]
    pub amount: u64,
    /// The mint to buy from. Accepted from the body because the corpus's request
    /// vector carries it; when absent a single configured mint is used.
    #[serde(default)]
    pub mint_url: Option<String>,
    #[serde(default)]
    pub unit: Option<String>,
    /// Wire compatibility only: identity comes from the network context (HTTP-03).
    #[serde(default)]
    pub mac: Option<String>,
}

#[derive(Debug, Serialize)]
struct InvoiceResponse {
    status: u8,
    quote: String,
    invoice: String,
    mint_url: String,
    amount: u64,
    expiry: u64,
    state: String,
    access_granted: bool,
}

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub quote: String,
    /// Wire compatibility only; the device identifier is resolved from the network.
    #[serde(default)]
    pub mac: Option<String>,
}

/// Success body for `GET` on a quote that has been applied.
#[derive(Debug, Serialize)]
struct PaidStatus {
    status: u8,
    quote: String,
    state: String,
    access_granted: bool,
    allotment: u64,
    metric: String,
}

/// Non-terminal `GET` body. `state` is the corpus's fixed vocabulary.
#[derive(Debug, Serialize)]
struct PendingStatus {
    status: u8,
    quote: String,
    state: String,
    access_granted: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    status: u8,
    error: &'a str,
}

fn registry(state: &AppState) -> Arc<Mutex<MintQuoteRegistry>> {
    state.quotes.clone()
}

fn json<T: Serialize>(
    code: StatusCode,
    body: &T,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    (
        code,
        [
            ("content-type", "application/json"),
            ("access-control-allow-origin", "*"),
        ],
        serde_json::to_string(body)
            .unwrap_or_else(|_| "{\"status\":0,\"error\":\"serialisation failed\"}".to_string()),
    )
}

fn err(code: StatusCode, msg: &str) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    // The corpus error envelope is `{status: 0, error}` — a bare string body is not
    // conformant, and a client parsing `status` would see nothing.
    json(
        code,
        &ErrorBody {
            status: 0,
            error: msg,
        },
    )
}

pub async fn handle_create_ln_invoice(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<CreateInvoiceRequest>,
) -> impl IntoResponse {
    // Corpus `post-missing-fields`: zero amount is a 400, not a quote.
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be greater than zero");
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Resolve which mint to buy from: the body's, else the single configured one.
    let mint_url = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet is not initialised");
        };
        match req
            .mint_url
            .clone()
            .map(|m| m.trim_end_matches('/').to_string())
            .or_else(|| w.default_mint().map(|m| m.to_string()))
        {
            Some(m) => m,
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "no mint_url given and no single configured mint to default to",
                )
            }
        }
    };

    // The mint must be one we accept. Charging against an unaccepted mint would take
    // money we cannot bank.
    if crate::http::routes::select::select_mint_for_token(&state.config, &mint_url).is_err() {
        return err(StatusCode::BAD_REQUEST, "mint_url is not an accepted mint");
    }

    let quote = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet unavailable");
        };
        match w.request_mint_quote(&mint_url, req.amount).await {
            Ok(q) => q,
            Err(e) => {
                // A mint failure must not be dressed up as a quote.
                tracing::error!(mint = %mint_url, error = %e, "mint quote request failed");
                return err(StatusCode::BAD_GATEWAY, "the mint did not issue a quote");
            }
        }
    };

    if quote.request.is_empty() {
        return err(
            StatusCode::BAD_GATEWAY,
            "the mint returned a quote without an invoice",
        );
    }

    let expiry = if quote.expiry > now {
        quote.expiry
    } else {
        now + DEFAULT_QUOTE_TTL_SECS
    };

    registry(&state)
        .lock()
        .map(|mut r| r.insert(&quote.id, req.amount, expiry))
        .ok();

    let body = InvoiceResponse {
        status: 1,
        quote: quote.id,
        invoice: quote.request,
        mint_url,
        amount: req.amount,
        expiry,
        state: "UNPAID".to_string(),
        // Nothing has been granted yet; claiming otherwise would unlock the gate for free.
        access_granted: false,
    };
    json(StatusCode::OK, &body)
}

/// Resolve the paying device from the request's address headers.
///
/// Extracted so the header plumbing is TESTABLE. The original bug was that the handler
/// called the resolver with three literal `None`s, and no test could see it because
/// identity resolution sat after minting and was unreachable without a live mint.
///
/// `resolve` is injected so a test can prove the header VALUES arrive, without needing a
/// DHCP lease on the test machine.
pub(crate) fn resolve_device(
    forwarded: Option<&str>,
    real_ip: Option<&str>,
    socket_ip: Option<&str>,
    resolve: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let ip = crate::http::mac::client_ip_from_headers(forwarded, real_ip, socket_ip)?;
    resolve(&ip)
}

pub async fn handle_get_ln_invoice(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Query(q): Query<InvoiceQuery>,
) -> impl IntoResponse {
    let found = registry(&state)
        .lock()
        .ok()
        .and_then(|r| r.get(&q.quote).cloned());

    // Corpus `get-unknown-quote-fullservice`: 404 with the error envelope.
    let Some(quote) = found else {
        return err(StatusCode::NOT_FOUND, "unknown quote");
    };

    let mint_url = {
        let guard = state.wallet.lock().await;
        guard
            .as_ref()
            .and_then(|w| w.default_mint().map(|m| m.to_string()))
    };

    // Ask the MINT for the truth rather than trusting our cached guess.
    let mut state_str = match quote.state {
        QuoteState::Unpaid => "UNPAID",
        QuoteState::Paid => "PAID",
        QuoteState::Issued => "ISSUED",
    }
    .to_string();

    if let Some(mint) = &mint_url {
        let guard = state.wallet.lock().await;
        if let Some(w) = guard.as_ref() {
            match w.check_mint_quote(mint, &quote.id).await {
                Ok(mint_state) => {
                    let upper = mint_state.to_uppercase();
                    state_str = if upper.contains("ISSUED") {
                        "ISSUED".to_string()
                    } else if upper.contains("PAID") {
                        "PAID".to_string()
                    } else if upper.contains("UNPAID") {
                        "UNPAID".to_string()
                    } else {
                        // Passed through verbatim: coercing an unknown state to UNPAID
                        // would tell a paying customer to pay again.
                        mint_state
                    };
                }
                Err(e) => {
                    tracing::warn!(quote = %quote.id, error = %e, "could not reach the mint");
                }
            }
        }
    }

    if state_str == "UNPAID" {
        let body = PendingStatus {
            status: 1,
            quote: quote.id.clone(),
            state: state_str,
            access_granted: false,
        };
        return json(StatusCode::OK, &body);
    }

    // ISSUED means we already minted for this quote. Re-minting returns
    // "Request already paid" from the mint, so this path must NOT call mint_tokens
    // again -- it reports the grant it already made. Idempotency lives here because
    // the client may poll GET repeatedly.
    if state_str == "ISSUED" {
        if let Ok(reg) = registry(&state).lock() {
            if let Some(_q) = reg.get(&quote.id) {
                // fall through to the grant report below
            }
        }
    }

    // PAID (or ISSUED): the money arrived, so MINT and GRANT.
    //
    // This is the step that was missing entirely. `mint_tokens` had never been called
    // from production code, so a customer could pay an invoice and get nothing.
    let Some(mint) = mint_url else {
        return err(StatusCode::SERVICE_UNAVAILABLE, "wallet has no mint");
    };

    let minted = {
        let guard = state.wallet.lock().await;
        let Some(w) = guard.as_ref() else {
            return err(StatusCode::SERVICE_UNAVAILABLE, "wallet unavailable");
        };
        if state_str == "ISSUED" {
            // Already minted in an earlier poll; do not ask the mint again.
            quote.amount
        } else {
            match w.mint_tokens(&mint, &quote.id).await {
                Ok(n) => n,
                Err(e) => {
                    // Paid but not yet applied: the corpus allows a 503 transient here, and
                    // retrying is correct. A 200 would lie about the grant.
                    tracing::warn!(quote = %quote.id, error = %e, "minting from a paid quote failed");
                    return err(
                        StatusCode::SERVICE_UNAVAILABLE,
                        &format!("payment received, tokens not yet issued: {e}; retry"),
                    );
                }
            }
        }
    };

    // Allotment: one step per sat at the default price, which is what the corpus's
    // `price_per_step = 1` means. Using the raw amount would over-grant when an operator
    // charges more than a sat per step.
    // `step_size` is a gateway-wide setting (Config), not per-mint.
    let step_size = state.config.step_size;
    let price_per_step = state
        .config
        .accepted_mints
        .iter()
        .find(|m| m.url.trim_end_matches('/') == mint.trim_end_matches('/'))
        .map(|m| m.price_per_step.max(1))
        .unwrap_or(1);
    let allotment = (minted / price_per_step) * step_size;

    // The device identifier comes from the network; `mac` in the query is accepted for
    // wire compatibility and deliberately ignored (HTTP-03), because it is
    // client-controlled and honouring it would let a device claim another's paid session.
    // Same resolution A3 established for /pay: address -> lease/ARP -> MAC.
    //
    // The three `None`s here were a real bug: the handler never received the request's
    // headers, so the resolver was called with no input and could never succeed. A paid
    // quote therefore always answered "the device could not be identified" and minted
    // nothing. `/pay` had the same information and worked; this handler simply did not
    // ask for it.
    //
    // The query `mac` is deliberately NOT consulted: it is client-controlled, and
    // honouring it would let a device claim another's paid session.
    let forwarded = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok());
    let real_ip = headers.get("x-real-ip").and_then(|v| v.to_str().ok());
    let socket_ip = headers
        .get("x-tollgate-socket-addr")
        .and_then(|v| v.to_str().ok());

    let device = resolve_device(forwarded, real_ip, socket_ip, |ip| {
        crate::http::mac::resolve_client_mac_default(ip).map(|r| r.mac)
    });
    let Some(mac) = device else {
        // Paid, but we cannot say who to. Refuse rather than grant to a sentinel.
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "payment received but the device could not be identified; retry from the client",
        );
    };

    {
        let mut sessions = state.sessions.lock().await;
        sessions.create_session(
            &mac,
            allotment,
            DEFAULT_METRIC,
            quote.expiry.saturating_sub(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
        );
    }
    registry(&state)
        .lock()
        .map(|mut r| r.mark_issued(&quote.id))
        .ok();

    let body = PaidStatus {
        status: 1,
        quote: quote.id,
        state: "ISSUED".to_string(),
        access_granted: true,
        allotment,
        metric: DEFAULT_METRIC.to_string(),
    };
    json(StatusCode::OK, &body)
}
