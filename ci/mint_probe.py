"""P2 -- live mint checks against a real Cashu mint.

Why this is a separate module from the runner: the runner's job is to *report*
verdicts honestly, and the one failure mode that would ruin it is a network error
being recorded as a pass. Isolating the mint contact here makes the rule enforceable
in one place: **anything we could not read is an error, never a verdict.**

The two rules that matter for correctness:

1. A NUT-07 checkstate must happen immediately before a spend. Cashu proofs are
   single-use; a stale "unspent" read is how you double-spend a fixture.
2. An unreadable spend state is NOT "unspent". Fail closed. A corpus twin marked
   UNSPENT is a known-good reference, and wrongly declaring it spent retires it
   from the corpus forever.

No `requests` dependency: stdlib only, so the runner stays runnable on a bare host.
"""

from __future__ import annotations

import base64
import json
import urllib.error
import urllib.request

# Corpus verdict vocabulary (corpus/manifest.json -> vocabularies.verify / .spend).
# An answer outside this set is an ERROR, not a verdict -- see `classify`.
KNOWN_VERDICTS = frozenset(
    {
        "ok",
        "spent",
        "unknown",
        "refused-degraded-unconsumed",
        "refused",
        "error",
    }
)


class MintError(RuntimeError):
    """The mint could not be read. Never convert this into a verdict."""


def _post_json(url: str, payload: dict, timeout: float = 20.0) -> dict:
    data = json.dumps(payload).encode()
    req = urllib.request.Request(
        url,
        data=data,
        method="POST",
        headers={
            "Content-Type": "application/json",
            # Cloudflare (error 1010) rejects python-urllib's default UA. A mint is
            # a public HTTP service; identifying ourselves plainly is enough.
            "User-Agent": "tollgate-conformance-runner/1.0",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:300]
        raise MintError(f"{url}: HTTP {e.code}: {detail}") from None
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        raise MintError(f"{url}: unreachable or unreadable: {e}") from None


def _get_json(url: str, timeout: float = 20.0) -> dict:
    try:
        # Cloudflare fronts several public mints and rejects python-urllib's
        # default User-Agent with HTTP 403 (error 1010). Identify plainly.
        req = urllib.request.Request(
            url, headers={"User-Agent": "tollgate-conformance-runner/1.0"}
        )
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read().decode())
    except urllib.error.HTTPError as e:
        detail = e.read().decode("utf-8", "replace")[:300]
        raise MintError(f"{url}: HTTP {e.code}: {detail}") from None
    except (urllib.error.URLError, TimeoutError, ValueError) as e:
        raise MintError(f"{url}: unreachable or unreadable: {e}") from None


def mint_info(mint_url: str) -> dict:
    """NUT-06 info. Raises MintError if the mint cannot be read."""
    return _get_json(mint_url.rstrip("/") + "/v1/info")


def supports(info: dict, nut: int) -> bool:
    return str(nut) in {str(k) for k in info.get("nuts", {})}


def decode_token(token: str):
    """Return (mint_url, proofs) for a Cashu token.

    Supports both wire generations:
      * `cashuA` -- base64 JSON (V1/V3 legacy)
      * `cashuB` -- base64 CBOR (V4)
    V4 is decoded by hand because there is no CBOR dependency here and adding one
    to a conformance runner is not worth it; the map shape is stable.
    """
    token = token.strip()
    if token.startswith("cashuA"):
        body = token[len("cashuA") :]
        body += "=" * (-len(body) % 4)
        d = json.loads(base64.urlsafe_b64decode(body))
        mint = d.get("m") or (d.get("t") or {}).get("m")
        proofs = d.get("proofs") or (d.get("t") or {}).get("p") or []
        return mint, proofs
    if token.startswith("cashuB"):
        raise MintError(
            "V4 (cashuB) token: CBOR decoding is not implemented in this runner; "
            "pass the mint explicitly with --mint and the proofs cannot be "
            "extracted. Recorded as UNVERIFIED rather than guessed."
        )
    raise MintError(f"unrecognised token format: {token[:12]!r}")


def check_state(mint_url: str, ys: list[str]) -> dict[str, str]:
    """NUT-07 checkstate for a list of proof secrets.

    Returns {Y: state}. Raises MintError if the mint did not answer -- a caller
    must not treat that as "unspent".
    """
    url = mint_url.rstrip("/") + "/v1/checkstate"
    resp = _post_json(url, {"Ys": ys})
    states = resp.get("states")
    if not isinstance(states, list):
        raise MintError(f"checkstate: no 'states' in response: {str(resp)[:200]}")
    out = {}
    for entry in states:
        y = entry.get("Y")
        st = entry.get("state")
        if y is None or st is None:
            raise MintError(f"checkstate: malformed entry {entry!r}")
        out[y] = st
    return out


def classify(checkstate: dict[str, str]) -> str:
    """Map a NUT-07 answer onto the corpus vocabulary.

    ALL_UNSPENT -> ok (proofs are spendable: a verification pass)
    anything SPENT -> spent
    a mixed/unknown state -> error, because the corpus vocabulary has no honest
    word for "I do not know" and inventing one is how a runner starts lying.
    """
    if not checkstate:
        raise MintError("checkstate returned no proofs")
    states = {s.upper() for s in checkstate.values()}
    if states == {"UNSPENT"}:
        return "ok"
    if "SPENT" in states:
        return "spent"
    if states <= {"UNSPENT", "PENDING"}:
        # PENDING is transient; not a certified pass.
        raise MintError(f"proofs in a transient state: {sorted(states)}")
    raise MintError(f"unrecognised checkstate vocabulary: {sorted(states)}")


def verdict_is_known(v: str) -> bool:
    return v in KNOWN_VERDICTS
