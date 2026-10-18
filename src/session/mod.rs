//! CustomerSession and SessionManager — session tracking.
//!
//! Two modes, deliberately:
//!
//! * `SessionManager::new()` is in-memory. Kept for tests and for callers that
//!   genuinely want a throwaway manager.
//! * `SessionManager::open(path)` is durable (see `store`). A process restart
//!   preserves a paid session; an expired one is closed rather than resurrected.
//!
//! The in-memory mode is NOT the production path. Losing a paid session on
//! restart silently cuts off a paying customer, which is why P1 exists.

pub mod store;

use std::collections::HashMap;
use std::path::Path;

pub use store::{SessionStore, StoreError, StoredQuote};

/// A single customer session keyed by MAC address.
#[derive(Debug, Clone)]
pub struct CustomerSession {
    /// Client MAC address — the primary key.
    pub mac: String,
    /// Total allotment in millisatoshis (time metric) or bytes (bytes metric).
    pub allotment: u64,
    /// How much has been consumed so far.
    pub used: u64,
    /// Metric type: "bytes" or "time".
    pub metric: String,
    /// Unix timestamp when the session expires.
    pub expiry: u64,
    /// Unix timestamp when the session was granted.
    pub granted_at: u64,
}

/// Session manager. `store == None` means in-memory (tests / throwaway);
/// `Some` means every mutation is durable.
pub struct SessionManager {
    pub sessions: HashMap<String, CustomerSession>,
    store: Option<SessionStore>,
}

impl SessionManager {
    /// Create a new empty in-memory SessionManager.
    pub fn new() -> Self {
        SessionManager {
            sessions: HashMap::new(),
            store: None,
        }
    }

    /// Open a durable manager at `path`.
    ///
    /// Recovery is conservative and explicit:
    /// * sessions whose expiry passed while we were down are **closed**, not
    ///   revived — restarting must not extend access;
    /// * still-valid sessions are reloaded with `used` intact — resetting it
    ///   would hand out free bytes on every restart;
    /// * unpaid quotes past their deadline are marked expired, never deleted
    ///   (a paid one must stay redeemable).
    ///
    /// Returns `Err` on a corrupt or unknown-schema store. Refusing to start is
    /// deliberate: starting empty would grant fresh access to every client.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let store = SessionStore::open(path)?;
        let now = now_secs();

        let closed = store.close_expired(now)?;
        for s in &closed {
            let down_for = now.saturating_sub(s.expiry);
            tracing::info!(
                mac = %s.mac,
                expired_for_secs = down_for,
                used = s.used,
                allotment = s.allotment,
                "closed session that expired while the daemon was down"
            );
        }
        let expired_quotes = store.expire_unpaid_quotes(now)?;
        if expired_quotes > 0 {
            tracing::info!(count = expired_quotes, "expired unpaid mint quotes");
        }

        let mut sessions = HashMap::new();
        for s in store.load_sessions()? {
            sessions.insert(s.mac.clone(), s);
        }

        Ok(SessionManager {
            sessions,
            store: Some(store),
        })
    }

    /// Whether this manager persists to disk.
    pub fn is_durable(&self) -> bool {
        self.store.is_some()
    }

    /// The underlying store, if durable. Used by the quote registry.
    pub fn store(&self) -> Option<&SessionStore> {
        self.store.as_ref()
    }

    /// Create and store a new session for the given MAC.
    /// Overwrites any existing session for the same MAC.
    pub fn create_session(
        &mut self,
        mac: &str,
        allotment: u64,
        metric: &str,
        duration_secs: u64,
    ) -> CustomerSession {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let session = CustomerSession {
            mac: mac.to_string(),
            allotment,
            used: 0,
            metric: metric.to_string(),
            expiry: now + duration_secs,
            granted_at: now,
        };
        self.sessions.insert(mac.to_string(), session.clone());
        if let Some(st) = &self.store {
            if let Err(e) = st.upsert_session(&session) {
                // A session we could not persist is a session we do not have:
                // remove it from memory so we never advertise unbacked access.
                self.sessions.remove(mac);
                tracing::error!(mac = %mac, error = %e, "failed to persist session; refusing to grant");
                return CustomerSession {
                    allotment: 0,
                    expiry: 0,
                    ..session
                };
            }
        }
        session
    }

    /// Look up a session by MAC address.
    pub fn get_session(&self, mac: &str) -> Option<&CustomerSession> {
        self.sessions.get(mac)
    }

    /// Check whether the session for `mac` is active (not expired, usage
    /// under allotment). Returns false if no session exists.
    pub fn is_active(&self, mac: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self.sessions.get(mac) {
            Some(s) => s.expiry > now && s.used < s.allotment,
            None => false,
        }
    }

    /// Remove a session by MAC. No-op if the MAC has no session.
    ///
    /// Deletion is persisted: a revoked session that reappeared after restart
    /// would be a re-grant nobody paid for.
    pub fn revoke_session(&mut self, mac: &str) {
        self.sessions.remove(mac);
        if let Some(st) = &self.store {
            if let Err(e) = st.delete_session(mac) {
                tracing::error!(mac = %mac, error = %e, "failed to delete revoked session");
            }
        }
    }

    /// Remove all expired sessions. Returns the number removed.
    pub fn cleanup_expired(&mut self) -> usize {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let expired_macs: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.expiry <= now)
            .map(|(mac, _)| mac.clone())
            .collect();
        let count = expired_macs.len();
        for mac in &expired_macs {
            self.sessions.remove(mac);
            if let Some(st) = &self.store {
                if let Err(e) = st.delete_session(mac) {
                    tracing::error!(mac = %mac, error = %e, "failed to delete expired session");
                }
            }
        }
        count
    }

    /// Update the `used` field for a session. No-op if session doesn't exist.
    pub fn update_usage(&mut self, mac: &str, used: u64) {
        if let Some(s) = self.sessions.get_mut(mac) {
            s.used = used;
            if let Some(st) = &self.store {
                if let Err(e) = st.set_usage(mac, used) {
                    tracing::error!(mac = %mac, used, error = %e, "failed to persist usage");
                }
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod store_test;

#[cfg(test)]
mod tests;
