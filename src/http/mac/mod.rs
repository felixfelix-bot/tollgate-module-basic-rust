//! Client identity resolution (A3).
//!
//! A TollGate keys sessions on the customer device's MAC, because the MAC is the
//! only identifier a captive-portal browser has — there are no accounts
//! (see the protocol README: "no accounts, no persistent identities, no
//! registration"). That makes this module load-bearing: if resolution returns a
//! sentinel, `/usage` cannot match the session and metering silently does
//! nothing.
//!
//! Resolution order and rationale:
//!   1. **dnsmasq lease file** — authoritative for DHCP clients.
//!      Format: `<expiry> <mac> <ip> <hostname> <clientid>`
//!   2. **kernel ARP table** — catches static-IP clients and survives a dnsmasq
//!      restart. In-memory, so it is a fallback and not a replacement.
//!      Format: `<ip> <hwtype> <flags> <mac> <mask> <device>`
//!
//! A `mac` supplied by the caller is never used as the identity. It is
//! client-controlled, so trusting it would let any device claim another's
//! session (and another's paid budget).

#[cfg(test)]
mod tests;

/// Which table produced the MAC. Reported so a caller can distinguish a
/// DHCP-registered device from one seen only in ARP, and so an operator can tell
/// why a device is (or is not) recognised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacSource {
    DhcpLease,
    Arp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMac {
    pub mac: String,
    pub source: MacSource,
}

/// Default locations on an OpenWrt router.
pub const DEFAULT_LEASE_PATH: &str = "/tmp/dhcp.leases";
pub const DEFAULT_ARP_PATH: &str = "/proc/net/arp";

/// Resolve `ip` to a MAC using the lease file, then ARP.
///
/// Returns `None` when the address is unparseable or absent from both tables —
/// deliberately *not* a sentinel: inventing an identity is what broke metering
/// before A3.
pub fn resolve_client_mac(
    ip: &str,
    lease_path: impl AsRef<std::path::Path>,
    arp_path: impl AsRef<std::path::Path>,
) -> Option<ResolvedMac> {
    let ip = ip.trim();
    // Reject anything that is not an address, so a hostname or a header value
    // cannot be smuggled in as one.
    let parsed: std::net::IpAddr = ip.parse().ok()?;
    let needle = parsed.to_string().to_lowercase();

    if let Ok(data) = std::fs::read_to_string(lease_path) {
        for line in data.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 && f[2].to_lowercase() == needle {
                return Some(ResolvedMac {
                    mac: f[1].to_lowercase(),
                    source: MacSource::DhcpLease,
                });
            }
        }
    }

    if let Ok(data) = std::fs::read_to_string(arp_path) {
        for line in data.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 4 && f[0].to_lowercase() == needle {
                let mac = f[3].to_lowercase();
                // An incomplete entry is all zeros; it is not an identity.
                if mac != "00:00:00:00:00:00" && mac != "00:00:00:00:00:00:00:00" {
                    return Some(ResolvedMac {
                        mac,
                        source: MacSource::Arp,
                    });
                }
            }
        }
    }

    None
}

/// Resolve using the router's default paths.
pub fn resolve_client_mac_default(ip: &str) -> Option<ResolvedMac> {
    resolve_client_mac(ip, DEFAULT_LEASE_PATH, DEFAULT_ARP_PATH)
}

/// Parse an `X-Forwarded-For` / `X-Real-Ip` style header into a bare address.
///
/// Only the *address* is taken from the header; the MAC still comes from the
/// tables above. A proxy in front of the daemon sets these legitimately.
pub fn client_ip_from_headers(
    forwarded_for: Option<&str>,
    real_ip: Option<&str>,
    socket_addr: Option<&str>,
) -> Option<String> {
    if let Some(f) = forwarded_for {
        if let Some(first) = f.split(',').next() {
            let t = first.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    if let Some(r) = real_ip {
        let t = r.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    socket_addr.map(|s| s.to_string())
}
