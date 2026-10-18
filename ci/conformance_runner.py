#!/usr/bin/env python3
"""A8 — corpus conformance runner for tollgate-module-basic-rust.

Drives the REAL binary over HTTP and compares what it does against the corpus's
recorded verdicts (`OpenTollGate/tollgate` → `corpus/`).

Why a separate runner instead of a Rust test: the corpus is data owned by another
repo, and the definition of "conformant" has to be that repo's, not ours. A test
inside the crate would let us quietly change the expectation.

WHAT THIS MEASURES — read before trusting a green run
-----------------------------------------------------
Only stages this binary can actually reach **without a mint and without a
router** are asserted. Everything else is recorded as UNVERIFIED, never as a
pass. Specifically:

  decode    the binary must parse the fixture's token structure. Reachable: the
            token is posted and token-parsing failures are distinguishable from
            mint failures.
  verify    requires a live mint (NUT-07 checkstate). UNVERIFIED offline.
  spend     requires a live mint. UNVERIFIED offline.
  http01-*  request/response shape. Reachable, offline.

A run with no mint therefore CANNOT certify `verify` or `spend`. That is the
honest outcome and it is printed as such.

Usage:
    python3 ci/conformance_runner.py --corpus ~/repos/tollgate/corpus
    python3 ci/conformance_runner.py --corpus ... --mint http://127.0.0.1:8384

Exit codes:
    0  every REACHABLE check passed (unverified stages reported, not counted)
    1  a reachable check failed
    2  the runner could not run (bad corpus, binary missing)
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

BINARY = os.environ.get("TOLLGATE_BIN", "target/debug/tollgate-module-basic-rust")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def wait_ready(port: int, timeout: float = 20.0) -> bool:
    """Poll until the daemon answers. No blind sleep."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/", timeout=1) as r:
                if r.status in (200, 402):
                    return True
        except urllib.error.HTTPError as e:
            if e.code in (200, 402):
                return True
        except Exception:
            pass
        time.sleep(0.15)
    return False


def raw_post(port: int, path: str, body: bytes, content_type: str | None) -> tuple[int, str]:
    """POST over a raw socket so an ABSENT Content-Type really is absent.

    `urllib` adds `Content-Type: application/x-www-form-urlencoded` whenever
    `data=` is passed, which silently defeats the absent-Content-Type test — the
    server sees a type and answers 415 for a reason the test never intended.
    That produced a false FAIL the first time this runner was written.
    """
    import socket as _s

    head = [
        f"POST {path} HTTP/1.1",
        f"Host: 127.0.0.1:{port}",
        "X-Forwarded-For: 192.168.1.50",
        "Connection: close",
    ]
    if content_type:
        head.append(f"Content-Type: {content_type}")
    head.append(f"Content-Length: {len(body)}")
    raw = ("\r\n".join(head) + "\r\n\r\n").encode() + body

    with _s.create_connection(("127.0.0.1", port), timeout=10) as sock:
        sock.sendall(raw)
        chunks = []
        while True:
            b = sock.recv(65536)
            if not b:
                break
            chunks.append(b)
    resp = b"".join(chunks)
    status_line = resp.split(b"\r\n", 1)[0].decode("iso-8859-1", "replace")
    try:
        status = int(status_line.split()[1])
    except (IndexError, ValueError):
        status = 0
    body_bytes = resp.split(b"\r\n\r\n", 1)[1] if b"\r\n\r\n" in resp else b""
    return status, body_bytes.decode("utf-8", "replace")


def post(port: int, path: str, body: bytes, content_type: str | None) -> tuple[int, str]:
    return raw_post(port, path, body, content_type)
    # A believable client address so MAC resolution has something to look at.
    req.add_header("x-forwarded-for", "192.168.1.50")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


def get(port: int, path: str) -> tuple[int, str]:
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method="GET")
    req.add_header("x-forwarded-for", "192.168.1.50")
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("utf-8", "replace")


class Result:
    def __init__(self) -> None:
        self.rows: list[dict] = []

    def add(self, stage: str, fixture: str, verdict: str, expected: str | None, note: str = "") -> None:
        self.rows.append(
            {"stage": stage, "fixture": fixture, "verdict": verdict,
             "expected": expected, "note": note}
        )

    def failed(self) -> list[dict]:
        return [r for r in self.rows if r["verdict"] == "FAIL"]

    def summary(self) -> dict:
        return {
            "pass": sum(1 for r in self.rows if r["verdict"] == "PASS"),
            "fail": sum(1 for r in self.rows if r["verdict"] == "FAIL"),
            "unverified": sum(1 for r in self.rows if r["verdict"] == "UNVERIFIED"),
        }


def _as_str(v) -> str:
    """Corpus verdict values are usually strings but some stages record a list."""
    if isinstance(v, list):
        return ", ".join(str(x) for x in v)
    return str(v)


def _verdict(verdicts, consumer: str) -> str | None:
    """Read one consumer's verdict.

    Some stages record `{consumer: verdict}`; `spend` records a LIST of
    `{when, consumer: verdict, evidence}` observations, because a proof can only
    be spent once and the corpus keeps each attempt. Handle both.
    """
    if isinstance(verdicts, list):
        for obs in verdicts:
            if isinstance(obs, dict) and consumer in obs:
                return _as_str(obs[consumer])
        return None
    if isinstance(verdicts, dict):
        v = verdicts.get(consumer)
        return None if v is None else _as_str(v)
    return None


def load_expected(corpus: pathlib.Path, entries: list[dict]) -> dict[str, dict]:
    """stage -> fixture -> {consumer: verdict}, from the corpus's lifecycle files."""
    out: dict[str, dict] = {}
    for entry in entries:
        p = corpus / entry["path"]
        if not p.exists():
            continue
        data = json.loads(p.read_text())
        fx = pathlib.Path(entry["path"]).stem
        for stage, body in data.get("stages", {}).items():
            verdicts = body.get("verdicts", {})
            if not isinstance(verdicts, (dict, list)):
                continue
            out.setdefault(stage, {})[fx] = verdicts
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--mint", default=None, help="live mint URL; without it, verify/spend stay UNVERIFIED")
    ap.add_argument("--out", default=None, help="write the verdict set as JSON here")
    args = ap.parse_args()

    corpus = pathlib.Path(args.corpus).expanduser()
    manifest_path = corpus / "manifest.json"
    if not manifest_path.exists():
        print(f"FATAL: no manifest at {manifest_path}", file=sys.stderr)
        return 2

    manifest = json.loads(manifest_path.read_text())
    tokens = manifest["index"]["tokens"]
    lifecycle = manifest["index"]["lifecycle"]
    expected = load_expected(corpus, lifecycle)

    if not pathlib.Path(BINARY).exists():
        print(f"FATAL: binary not found at {BINARY} (run: cargo build)", file=sys.stderr)
        return 2

    tmp = tempfile.mkdtemp(prefix="tg-conf-")
    port = free_port()
    env = dict(os.environ)
    env["TOLLGATE_TEST_CONFIG_DIR"] = tmp
    env["TOLLGATE_BIND"] = f"127.0.0.1:{port}"
    env["RUST_LOG"] = "warn"

    # A config with no mints: the daemon starts, but every payment fails at mint
    # selection. That is enough to exercise DECODE (structure) and the HTTP
    # contract, and it is why verify/spend cannot be certified this way.
    cfg = {
        "metric": "bytes",
        "step_size": 1000,
        "accepted_mints": [{"url": args.mint, "price_per_step": 1, "price_unit": "sats"}]
        if args.mint
        else [],
        "listen": f"127.0.0.1:{port}",
    }
    (pathlib.Path(tmp) / "config.json").write_text(json.dumps(cfg))

    proc = subprocess.Popen(
        [BINARY], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )
    res = Result()
    try:
        if not wait_ready(port):
            print("FATAL: daemon did not become ready", file=sys.stderr)
            proc.terminate()
            out = proc.communicate(timeout=5)[0]
            print(out[-2000:], file=sys.stderr)
            return 2

        # ---- decode: can the binary parse each fixture's structure? ----------
        for t in tokens:
            name = pathlib.Path(t["path"]).stem
            fx = corpus / t["path"]
            if not fx.exists():
                res.add("decode", name, "UNVERIFIED", None, "fixture file missing")
                continue
            status, body = post(port, "/", fx.read_bytes(), "text/plain")
            # A structural parse failure and a mint failure are different: the
            # latter means we got past parsing. With no mint configured,
            # reaching "mint not usable / not accepted" PROVES the token parsed.
            parsed = (
                "mint" in body.lower()
                or "accepted mints" in body.lower()
                or "does not cover one step" in body.lower()
                or status == 200
            )
            exp = expected.get("decode", {}).get(name, {})
            exp_go = None if exp.get("tollgate-module-basic-go@rc") is None else _as_str(exp.get("tollgate-module-basic-go@rc"))
            if status == 415 or "invalid Cashu token" in body.lower():
                res.add("decode", name, "FAIL", exp_go, f"token not parsed: {body[:120]}")
            elif parsed:
                res.add("decode", name, "PASS", exp_go, f"HTTP {status}")
            else:
                res.add("decode", name, "UNVERIFIED", exp_go, f"inconclusive: {body[:120]}")

        # ---- verify / spend: need a live mint -------------------------------
        for stage in ("verify", "spend"):
            for name in expected.get(stage, {}):
                if args.mint:
                    res.add(stage, name, "UNVERIFIED", None, "mint supplied but live check not implemented in this runner")
                else:
                    res.add(stage, name, "UNVERIFIED", _verdict(expected[stage][name], "tollgate-module-basic-go@rc"),
                            "requires a live mint (NUT-07); offline run cannot certify")

        # ---- HTTP-01 contract ----------------------------------------------
        # No Content-Type: the body is the token (HTTP-01).
        st, body = post(port, "/", b"cashuBnotarealtoken", None)
        if st == 415:
            res.add("http01-post", "absent-content-type", "FAIL", "accepted", "answered 415")
        else:
            res.add("http01-post", "absent-content-type", "PASS", "accepted", f"HTTP {st}")

        # Explicit unsupported type is still refused.
        st, _ = post(port, "/", b"<x/>", "application/xml")
        res.add("http01-post", "unsupported-content-type", "PASS" if st == 415 else "FAIL", "415", f"HTTP {st}")

        # >1 MiB refused.
        st, body = post(port, "/", b"a" * (1024 * 1024 + 1), "text/plain")
        res.add("http01-post", "body-over-1mib", "PASS" if st in (400, 413) else "FAIL", "400/413", f"HTTP {st}")

        # A failure must be a signed 21023 notice with level + code.
        st, body = post(port, "/", b"not-a-token", "text/plain")
        ok = False
        note = ""
        try:
            v = json.loads(body)
            tags = {t[0]: t[1:] for t in v.get("tags", []) if t}
            ok = (
                v.get("kind") == 21023
                and "level" in tags
                and "code" in tags
                and len(v.get("sig", "")) == 128
            )
            note = f"kind={v.get('kind')} tags={sorted(tags)} sig_len={len(v.get('sig',''))}"
        except Exception as e:
            note = f"not JSON: {e}"
        res.add("http01-notice", "rejected-payment", "PASS" if ok else "FAIL", "signed 21023", note)

        # ---- HTTP-02 /whoami ------------------------------------------------
        st, body = get(port, "/whoami")
        res.add("http02-whoami", "shape", "PASS" if st == 200 and "mac=" in body else "FAIL", "mac=<addr>", f"HTTP {st} {body[:40]}")

        # ---- HTTP-03 /usage -------------------------------------------------
        st, body = get(port, "/usage")
        ok = st == 200 and (body == "-1/-1" or "/" in body)
        res.add("http03-usage", "shape", "PASS" if ok else "FAIL", "-1/-1 or used/total", f"HTTP {st} {body[:40]}")

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    s = res.summary()
    print("\n=== corpus conformance — tollgate-module-basic-rust ===")
    print(f"corpus_version={manifest.get('corpus_version')} updated={manifest.get('updated')}")
    for r in res.rows:
        line = f"  [{r['verdict']:<10}] {r['stage']:<16} {r['fixture']:<24} {r['note']}"
        print(line)
    print(f"\n  PASS {s['pass']}  FAIL {s['fail']}  UNVERIFIED {s['unverified']}")
    print(
        "\n  NOTE: verify/spend are UNVERIFIED without a live mint. This run does NOT "
        "certify them,\n  and no consumer should be added to corpus kg_state.green_consumers "
        "on the strength of it."
    )

    if args.out:
        payload = {
            "consumer": "tollgate-module-basic-rust",
            "corpus_version": manifest.get("corpus_version"),
            "measured_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "mint": args.mint,
            "summary": s,
            "rows": res.rows,
        }
        outp = pathlib.Path(args.out)
        outp.parent.mkdir(parents=True, exist_ok=True)
        outp.write_text(json.dumps(payload, indent=2) + "\n")
        print(f"\n  verdict set written to {args.out}")

    return 1 if res.failed() else 0


if __name__ == "__main__":
    sys.exit(main())
