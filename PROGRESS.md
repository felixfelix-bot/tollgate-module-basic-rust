# Path A progress — tollgate-module-basic-rust

One line per item: `A<n> | finding | status | files touched`

A1 | 1022 was unsigned hand-built JSON, allotment was `received*1000` (unrelated to the price table) | DONE | src/http/routes/pay.rs, src/http/routes/pay_session_test.rs, src/nostr_event.rs
A2 | every failure path emitted bare {kind,content} JSON with no level/code/sig | DONE | src/http/routes/pay.rs, src/http/routes/pay_notice_test.rs, src/http/routes/mod.rs
A4 | absent Content-Type answered 415; String extractor unbounded (no 1 MiB cap) | DONE | src/http/routes/pay.rs, src/http/routes/pay_body_test.rs, src/http/routes/mod.rs
