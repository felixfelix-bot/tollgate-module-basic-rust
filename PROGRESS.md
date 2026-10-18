# Path A progress — tollgate-module-basic-rust

One line per item: `A<n> | finding | status | files touched`

A1 | 1022 was unsigned hand-built JSON, allotment was `received*1000` (unrelated to the price table) | DONE | src/http/routes/pay.rs, src/http/routes/pay_session_test.rs, src/nostr_event.rs
A2 | every failure path emitted bare {kind,content} JSON with no level/code/sig | DONE | src/http/routes/pay.rs, src/http/routes/pay_notice_test.rs, src/http/routes/mod.rs
A4 | absent Content-Type answered 415; String extractor unbounded (no 1 MiB cap) | DONE | src/http/routes/pay.rs, src/http/routes/pay_body_test.rs, src/http/routes/mod.rs
A3 | sessions keyed on the 00:00:00:00:00:00 sentinel; /usage keyed on an IP => never matched; metering dead | DONE | src/http/mac/{mod,tests}.rs, src/http/routes/{pay,usage,whoami}.rs, src/http/mod.rs
A7 | metering module never called: `/usage` always reported 0 used | DONE (wired, not deleted) | src/metering/{mod,wiring_test}.rs, src/http/routes/usage.rs
A5 | /ln-invoice was a stub: fake quote ids, hardcoded unpaid, expiry 0 | DONE (state machine; invoice fetch still needs a bound CDK wallet) | src/http/routes/{quote,ln_invoice,ln_invoice_quote_test}.rs, src/http/mod.rs, src/main.rs, src/cli/mod.rs, src/http/routes/discovery_test.rs, Cargo.toml
A6 | accepted_mints.first() both priced payments and gated acceptance: 2nd-mint tokens mispriced | DONE (selection; wallet still single-instance) | src/http/routes/{select,select_test,pay}.rs, src/wallet/verify.rs, src/http/routes/mod.rs
A9 | 3 tests in usage.rs compared literals to literals and never called the handler | DONE | src/http/routes/usage.rs
A8 | no way to measure the binary against the corpus; runner found a 30s hang on unaccepted mints | DONE | ci/conformance_runner.py, conformance/verdicts.json, src/main.rs, src/http/routes/pay.rs
P1 | sessions/quotes were in-memory: restart lost every paid session and every unpaid quote | DONE (SQLite WAL+FULL, fail-closed) | src/session/{store,store_test}.rs, src/session/mod.rs, src/main.rs, src/cli/mod.rs, Cargo.toml
P2 | runner reported verify/spend UNVERIFIED even with a mint; no live lane existed | DONE (mint reachable + NUT-07 detected; per-fixture proof check needs V4 CBOR) | ci/mint_probe.py, ci/test_mint_probe.py, ci/conformance_runner.py, conformance/verdicts.json
REMEASURE | corpus KG twins regenerated from a reachable mint (tollgate#47); verify/spend became measurable | PASS 13->15, UNVERIFIED 14->12 | conformance/verdicts.json, REPORT.md
ADR-0002 | usage counter paid a fsync per metering tick (money rule applied to a cache) | DONE (5s OR 5% OR terminal flush) | src/session/{mod,store,usage_batching_test}.rs
P3 | /ln-invoice invented quote ids and returned an empty request, so no client could pay | DONE (real BOLT11 from the mint) | src/http/routes/{ln_invoice,ln_invoice_live_test}.rs, src/wallet/wallet.rs
P4 | two-mint behaviour was untested; the code already keyed wallets by mint | DONE (regression tests, no code change) | src/wallet/{per_mint_test,mod}.rs
P5 | /ln-invoice shape did not match corpus; PAID quote never minted or granted | PARTIAL: shape fixed, grant hardware-gated | src/http/routes/{ln_invoice,ln_invoice_contract_test}.rs, src/wallet/wallet.rs
P5a | aarch64 musl cross-build was recorded as blocked; it is not - musl.cc toolchain works | DONE (verified on hardware) | ci/cross-build-aarch64.sh
P5b | /balance returned a wallet dump, not the corpus client-session shape; found on hardware | DONE (verified on device) | src/http/routes/{balance,balance_contract_test}.rs
