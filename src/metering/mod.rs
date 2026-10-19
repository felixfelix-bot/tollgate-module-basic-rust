//! Metering — ndsctl integration for usage polling.
//!
//! Calls `ndsctl json <mac>` to read a client's current download/upload bytes.
//!
//! MEASURED ON HARDWARE 2026-10-19: the previous implementation invoked
//! `ndsctl state <mac>`, and nodogsplash 5.0.2 has NO `state` command --
//! `ndsctl` answers "Unknown command: state". The parser then found no
//! `download:`/`upload:` lines and returned (0, 0) forever, so metering reported
//! zero usage for every client regardless of traffic. Because (0,0) is also the
//! legitimate answer for a client with no traffic, the failure was indistinguishable
//! from "nothing has been transferred yet" -- the same silent-zero class of bug A3
//! and A7 were supposed to have removed.
//!
//! The correct interface is `ndsctl json <mac>`, which returns an object with
//! `downloaded` / `uploaded` byte counts. Note it returns `{}` for a MAC the
//! managed interface has never seen, which is an ABSENCE and must not be read as
//! zero usage.

use thiserror::Error;

/// Errors from metering operations.
#[derive(Debug, Error)]
pub enum MeteringError {
    #[error("ndsctl not found: {0}")]
    NotFound(String),
    #[error("ndsctl failed: {0}")]
    ExecutionFailed(String),
    #[error("parse error: {0}")]
    ParseError(String),
}

/// Parse ndsctl output to extract used (download + upload) bytes.
/// Expected format lines like:
/// ```text
/// download: 1234567
/// upload: 7654321
/// ```
/// Returns (used, total) where `used` = download + upload and
/// `total` = 0 (ndsctl doesn't report total allotment).
pub fn parse_ndsctl_output(output: &str) -> Result<(u64, u64), MeteringError> {
    let mut download: u64 = 0;
    let mut upload: u64 = 0;

    for line in output.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("download:") {
            download = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("upload:") {
            upload = val.trim().parse().unwrap_or(0);
        }
    }

    Ok((download + upload, 0))
}

/// Parse `ndsctl json <mac>` output.
///
/// MEASURED on nodogsplash 5.0.2 (tollgate-A04E, 2026-10-19) -- the real shape is FLAT:
/// ```json
/// {"id":1,"ip":"192.168.1.159","mac":"50:76:af:2f:33:ed","state":"Preauthenticated",
///  "downloaded":0,"uploaded":0,"avg_down_speed":0.00,"avg_up_speed":0.00,...}
/// ```
/// An earlier version of this function expected `{"clients":{"<mac>":{...}}}`. That was
/// invented, not observed, and made the parser return `None` for every real response.
///
/// Returns `Ok(None)` when the answer is `{}`: nodogsplash uses that for a client it does
/// not know. That is ABSENCE, not "transferred nothing" -- folding the two together is
/// what let metering report a confident zero.
pub fn parse_ndsctl_json(output: &str) -> Result<Option<(u64, u64)>, MeteringError> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MeteringError::ParseError(e.to_string()))?;

    // Flat object: read the counters directly. A client that is present but has
    // transferred nothing legitimately reports 0 -- that is a REAL zero and must parse.
    let down = v.get("downloaded").and_then(|x| x.as_u64());
    let up = v.get("uploaded").and_then(|x| x.as_u64());
    match (down, up) {
        (Some(d), Some(u)) => Ok(Some((d.saturating_add(u), 0))),
        // No counter fields at all: this is not a client record, so we cannot claim
        // anything about usage. Absence, not zero.
        _ => Ok(None),
    }
}

/// Poll usage for a client MAC via `ndsctl json <mac>`.
/// Returns (used_bytes, total_bytes). On any error, returns (0, 0).
pub async fn poll_usage(mac: &str) -> Result<(u64, u64), MeteringError> {
    let output = tokio::process::Command::new("ndsctl")
        .args(["json", mac])
        .output()
        .await
        .map_err(|e| MeteringError::NotFound(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(MeteringError::ExecutionFailed(stderr.to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // `{}` from ndsctl means the managed interface has never seen this MAC. That is an
    // ABSENCE, not zero usage: reporting (0,0) would claim "this client transferred
    // nothing", which is exactly the silent-zero failure this fix exists to remove.
    match parse_ndsctl_json(&stdout)? {
        Some((used, total)) => Ok((used, total)),
        None => Err(MeteringError::ParseError(format!(
            "ndsctl has no record of {mac} on the managed interface (returned {})",
            stdout.trim()
        ))),
    }
}

/// A usage reading plus its provenance.
///
/// `live` distinguishes "the enforcement daemon said so" from "the stored
/// counter is the best we have". They are different claims and `/usage` must not
/// present them as the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageReading {
    pub used: u64,
    pub live: bool,
}

/// Poll usage using a caller-supplied `ndsctl` path.
///
/// Absence or failure is an `Err`, never a zero: reporting `used = 0` for a
/// client the daemon does not know is indistinguishable from a client that has
/// genuinely transferred nothing, and it hides a broken enforcement path.
pub async fn poll_usage_with(
    ndsctl: impl AsRef<std::path::Path>,
    mac: &str,
) -> Result<UsageReading, MeteringError> {
    let output = tokio::process::Command::new(ndsctl.as_ref())
        .args(["state", mac])
        .output()
        .await
        .map_err(|e| MeteringError::NotFound(e.to_string()))?;

    if !output.status.success() {
        return Err(MeteringError::ExecutionFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }

    let (used, _total) = parse_ndsctl_output(&String::from_utf8_lossy(&output.stdout))?;
    Ok(UsageReading { used, live: true })
}

/// Poll using the router's `ndsctl`.
pub async fn poll_usage_reading(mac: &str) -> Result<UsageReading, MeteringError> {
    poll_usage_with("ndsctl", mac).await
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod wiring_test;

#[cfg(test)]
mod ndsctl_json_tests {
    use super::*;

    /// The real response, captured verbatim from nodogsplash 5.0.2 on tollgate-A04E.
    /// Flat object, not the nested `{"clients":{...}}` an earlier version assumed.
    const MEASURED: &str = r#"{
    "id":1,
    "ip":"192.168.1.159",
    "mac":"50:76:af:2f:33:ed",
    "added":0,
    "active":1792420671,
    "duration":0,
    "token":"47d4a1bf",
    "state":"Preauthenticated",
    "downloaded":1200,
    "avg_down_speed":0.00,
    "uploaded":300,
    "avg_up_speed":0.00
    }"#;

    #[test]
    fn parses_the_measured_flat_shape() {
        let got = parse_ndsctl_json(MEASURED).expect("must parse");
        assert_eq!(got, Some((1500, 0)), "used = downloaded + uploaded");
    }

    /// The invented nested shape must NOT be silently accepted as a client record.
    #[test]
    fn nested_clients_shape_is_not_a_client_record() {
        let invented = r#"{"clients":{"aa:bb:cc:dd:ee:ff":{"downloaded":5,"uploaded":5}}}"#;
        assert_eq!(
            parse_ndsctl_json(invented).expect("parses as JSON"),
            None,
            "the nested shape was invented; real ndsctl is flat"
        );
    }

    /// `{}` means nodogsplash does not know this client -- absence, not zero.
    #[test]
    fn empty_object_is_absence_not_zero() {
        assert_eq!(parse_ndsctl_json("{}").expect("parses"), None);
        assert_eq!(parse_ndsctl_json("").expect("parses"), None);
    }

    /// A present client with no traffic IS a real zero.
    #[test]
    fn present_client_with_no_traffic_is_a_real_zero() {
        let out =
            r#"{"mac":"aa:bb:cc:dd:ee:ff","state":"Preauthenticated","downloaded":0,"uploaded":0}"#;
        assert_eq!(parse_ndsctl_json(out).expect("parses"), Some((0, 0)));
    }

    #[test]
    fn malformed_output_is_an_error() {
        assert!(parse_ndsctl_json("Unknown command: state").is_err());
    }

    /// The legacy `download:`/`upload:` lines came from `ndsctl state`, a command that
    /// does not exist. Those lines must not be accepted as a client record.
    #[test]
    fn legacy_line_format_is_not_accepted() {
        assert!(parse_ndsctl_json("download: 1234\nupload: 5678").is_err());
    }
}
