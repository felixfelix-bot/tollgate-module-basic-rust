# Path A progress — tollgate-module-basic-rust

One line per item: `A<n> | finding | status | files touched`

A1 | 1022 was unsigned hand-built JSON, allotment was `received*1000` (unrelated to the price table) | DONE | src/http/routes/pay.rs, src/http/routes/pay_session_test.rs, src/nostr_event.rs
A2 | every failure path emitted bare {kind,content} JSON with no level/code/sig | DONE | src/http/routes/pay.rs, src/http/routes/pay_notice_test.rs, src/http/routes/mod.rs
A4 | absent Content-Type answered 415; String extractor unbounded (no 1 MiB cap) | DONE | src/http/routes/pay.rs, src/http/routes/pay_body_test.rs, src/http/routes/mod.rs
A3 | sessions keyed on the 00:00:00:00:00:00 sentinel; /usage keyed on an IP => never matched; metering dead | DONE | src/http/mac/{mod,tests}.rs, src/http/routes/{pay,usage,whoami}.rs, src/http/mod.rs
A7 | metering module never called: `/usage` always reported 0 used | DONE (wired, not deleted) | src/metering/{mod,wiring_test}.rs, src/http/routes/usage.rs
