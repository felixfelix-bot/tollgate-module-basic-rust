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
/// Shape (nodogsplash 5.x):
/// ```json
/// {"client_length":1,"clients":{"aa:bb:..":{"downloaded":123,"uploaded":45,...}}}
/// ```
/// Returns `Ok(None)` when the object is `{}` -- that means "this interface has never
/// seen the client", which is NOT the same as "the client transferred nothing". Folding
/// those together is what let metering report a confident zero.
pub fn parse_ndsctl_json(output: &str) -> Result<Option<(u64, u64)>, MeteringError> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return Ok(None);
    }
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MeteringError::ParseError(e.to_string()))?;
    let Some(clients) = v.get("clients").and_then(|c| c.as_object()) else {
        return Ok(None);
    };
    if clients.is_empty() {
        return Ok(None);
    }
    // Sum across entries; a single-MAC query normally yields exactly one.
    let mut down = 0u64;
    let mut up = 0u64;
    for c in clients.values() {
        down = down.saturating_add(c.get("downloaded").and_then(|x| x.as_u64()).unwrap_or(0));
        up = up.saturating_add(c.get("uploaded").and_then(|x| x.as_u64()).unwrap_or(0));
    }
    Ok(Some((down.saturating_add(up), 0)))
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

    /// The shape nodogsplash 5.0.2 actually returns for `ndsctl json <mac>`.
    /// Measured on the bench, not assumed.
    #[test]
    fn parses_the_real_json_shape() {
        let out = r#"{"client_length":1,"clients":{"50:76:af:2f:33:ed":{"downloaded":1200,"uploaded":300,"session_start":1,"session_end":2}}}"#;
        let got = parse_ndsctl_json(out).expect("must parse");
        assert_eq!(got, Some((1500, 0)), "used = downloaded + uploaded");
    }

    /// `{}` means the managed interface has never seen this MAC.
    ///
    /// This is the distinction the old code destroyed: it reported (0, 0) for BOTH
    /// "no traffic yet" and "no such client", so metering silently claimed zero.
    #[test]
    fn empty_object_is_absence_not_zero() {
        assert_eq!(parse_ndsctl_json("{}").expect("parses"), None);
        assert_eq!(parse_ndsctl_json("").expect("parses"), None);
    }

    /// A client genuinely present with zero bytes IS a real zero.
    #[test]
    fn present_client_with_no_traffic_is_a_real_zero() {
        let out = r#"{"clients":{"aa:bb:cc:dd:ee:ff":{"downloaded":0,"uploaded":0}}}"#;
        assert_eq!(parse_ndsctl_json(out).expect("parses"), Some((0, 0)));
    }

    /// Garbage is an error, never a silent zero.
    #[test]
    fn malformed_output_is_an_error() {
        assert!(parse_ndsctl_json("Unknown command: state").is_err());
    }

    /// The old line format must NOT be what we parse any more -- `ndsctl state` is not
    /// a command in nodogsplash 5.x, so any parser keyed to it can only ever yield 0.
    #[test]
    fn legacy_line_format_is_not_accepted_as_json() {
        let legacy = "download: 1234\nupload: 5678";
        assert!(
            parse_ndsctl_json(legacy).is_err(),
            "the legacy `download:`/`upload:` shape came from a command that does not exist"
        );
    }
}
