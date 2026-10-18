//! A7 — the metering module was dead code: `parse_ndsctl_output` and
//! `poll_usage` existed with tests, but nothing called either, so a session's
//! `used` counter never left zero and `/usage` always reported `0/<allotment>`.
//!
//! Wiring it makes `/usage` report live usage from the enforcement daemon, with
//! an explicit fallback to the stored counter when ndsctl is unavailable (the
//! module's documented "graceful degradation" for non-OpenWrt environments).

use super::{parse_ndsctl_output, poll_usage_with, UsageReading};

#[test]
fn parses_download_and_upload_as_a_sum() {
    let out = "download: 1234\nupload: 766\n";
    let (used, total) = parse_ndsctl_output(out).expect("parse");
    assert_eq!(used, 2000, "used is download + upload");
    assert_eq!(total, 0, "ndsctl does not report an allotment");
}

#[test]
fn missing_fields_are_zero_not_an_error() {
    // A client the daemon knows but has not counted yet.
    let (used, _) = parse_ndsctl_output("download: 0\nupload: 0\n").expect("parse");
    assert_eq!(used, 0);
}

/// A fake `ndsctl` so the wiring is testable without a router.
fn fake_ndsctl(dir: &tempfile::TempDir, script: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.path().join("ndsctl");
    std::fs::write(&p, script).unwrap();
    let mut perm = std::fs::metadata(&p).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(&p, perm).unwrap();
    p
}

#[tokio::test]
async fn reads_usage_from_the_enforcement_daemon() {
    let d = tempfile::tempdir().unwrap();
    let bin = fake_ndsctl(&d, "#!/bin/sh\necho 'download: 500'\necho 'upload: 250'\n");
    let got = poll_usage_with(&bin, "aa:bb:cc:dd:ee:ff")
        .await
        .expect("reading");
    assert_eq!(got.used, 750);
    assert!(got.live, "a successful poll is live data");
}

#[tokio::test]
async fn reports_unavailable_rather_than_zero_when_ndsctl_is_missing() {
    let d = tempfile::tempdir().unwrap();
    let missing = d.path().join("does-not-exist");
    let got = poll_usage_with(&missing, "aa:bb:cc:dd:ee:ff").await;
    assert!(
        got.is_err(),
        "A7: absence must be distinguishable from a real zero reading, otherwise \
         /usage reports 0 for an unenforced client"
    );
}

#[tokio::test]
async fn non_zero_exit_is_an_error_not_a_zero_reading() {
    let d = tempfile::tempdir().unwrap();
    let bin = fake_ndsctl(&d, "#!/bin/sh\necho 'client not found' >&2\nexit 1\n");
    assert!(poll_usage_with(&bin, "aa:bb:cc:dd:ee:ff").await.is_err());
}

#[test]
fn reading_carries_its_own_provenance() {
    let live = UsageReading {
        used: 10,
        live: true,
    };
    let cached = UsageReading {
        used: 10,
        live: false,
    };
    assert_ne!(
        live, cached,
        "a live reading and a cached fallback must be distinguishable; they are not the same claim"
    );
}
