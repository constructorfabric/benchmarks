# Vendored from spec-bench grading/seat_budget.py @ 21955bb (unchanged below this line).
"""How much of the subscription seat's quota is left, and whether a trial may start.

A run must never reach the account's extra usage credits: they stay enabled for the human
working on the same seat, so the harness has to stop itself rather than rely on a cap. The
only live source for "how much is left" is the rate-limit header block Anthropic returns on
every `POST /v1/messages` — three windows (`5h`, `7d`, `overage`), each with `-status`,
`-reset` (unix seconds) and `-utilization` (0..1).

The gate reads the two subscription windows and stops at ``BUDGET_MAX`` of the tighter one.
`overage-utilization` (the monthly credit budget, present only while credits are enabled) is
reported for visibility but never decides: stopping at 95% of the window means credits are
never reached in the first place.

Probing costs 8 input + 1 output tokens and does not move `utilization`, which is rounded to
two decimals — so the real stop threshold is 95.0–95.5%, not exactly 95%.

**Fail-open by design.** A probe that fails, a non-200, or a changed header contract all
return ``go``: the probe and the trial talk to the same API, so a probe that cannot get an
answer almost always means the agent cannot work either — and blocking the run there would
turn an API hiccup into a stalled overnight round. The one exception is HTTP 429: that IS
the answer — the seat refuses the probed model right now, so a trial started on it dies the
same way (model-specific caps produce exactly this with both windows reading as free).

Behaviour, verified against fixtures and one live seat (2026-08-12):

| headers                                  | verdict                        |
|------------------------------------------|--------------------------------|
| 5h 0.20 / 7d 0.40                        | go, headroom 0.60              |
| 5h 0.96 / 7d 0.40                        | wait until the 5h reset        |
| 5h 0.10 / 7d 0.97                        | wait until the 7d reset        |
| 5h 0.95 / 7d 0.10 (exactly at the bound) | wait — the bound is inclusive  |
| no `-utilization` headers                | go + warning (contract change) |
| HTTP 429                                 | wait — the seat refused the probe itself |
| HTTP 401 / probe failed                  | go + warning                   |

Every fail-open answer warns on stderr, and both callers pass that through to the run log:
a gate that is off has to be visible in the transcript of the run it failed to guard.

Used by scripts/run_experiment.sh (watchdog during a batch) and scripts/run_matrix.sh
(pre-flight), which read the one-line CLI output below. The line carries the 5h window's own
headroom and reset after the binding window's, so a caller deciding whether a long trial fits
can size it against the window that actually refills within the hour — see ``Verdict``.
"""

from __future__ import annotations

import json
import os
import sys
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

API_URL = "https://api.anthropic.com/v1/messages"

#: Subscription windows the gate is allowed to spend. `overage` is deliberately absent.
WINDOWS = ("5h", "7d")

#: Stop once the tighter window is this full. 5% is roughly one B5 trial on a fresh seat.
DEFAULT_BUDGET_MAX = 0.95

#: Where the HTTP status is kept alongside the headers. A colon cannot appear in a real
#: header name, so this can never collide with one.
STATUS = ":status"

_HEADER = "anthropic-ratelimit-unified-{window}-{field}"
_OVERAGE = "anthropic-ratelimit-unified-overage-utilization"

#: Cheap default for callers that only watch the unified windows. Callers guarding a run
#: should pass the run's own model instead: seats carry model-specific caps that only a
#: request to that model trips.
DEFAULT_PROBE_MODEL = "claude-haiku-4-5"


def _probe_body(model: str) -> bytes:
    """Smallest request that still returns the header block."""
    return json.dumps(
        {
            "model": model,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "."}],
        }
    ).encode()


@dataclass(frozen=True)
class Verdict:
    """``go`` or ``wait``; the rest is why, and until when."""

    action: str
    reason: str
    reset: int | None = None
    headroom: float | None = None
    overage: float | None = None
    #: Free share of the 5h window specifically, and when it resets. ``headroom`` above is the
    #: tighter of the two windows, which is the right answer for "may a trial start" but the
    #: wrong one for "does a long trial fit before quota returns": once the 7d window is the
    #: tighter one, its headroom stops moving for days while the 5h window keeps refilling
    #: hourly. A caller sizing a trial against the rolling window reads these two instead.
    headroom_5h: float | None = None
    reset_5h: int | None = None
    #: True when this ``go`` is the fail-open default rather than a measured answer — the gate
    #: is off for this poll, and the caller must say so instead of logging nothing.
    fail_open: bool = False


def _num(value: str | None) -> float | None:
    try:
        return float(value)  # type: ignore[arg-type]
    except (TypeError, ValueError):
        return None


def _headers_of(status: int, message) -> dict[str, str]:
    out = {k.lower(): v.strip() for k, v in message.items()}
    out[STATUS] = str(status)
    return out


def _fake_headers(path: str) -> dict[str, str] | None:
    """Headers read from a file instead of the network (SEAT_BUDGET_FAKE_HEADERS).

    Accepts exactly what `curl -D -` writes, so a real response can be captured once and
    then edited to rehearse a threshold the live seat is nowhere near.
    """
    try:
        lines = Path(path).read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return None
    out = {STATUS: "200"}
    for line in lines:
        line = line.strip()
        if line.upper().startswith("HTTP/"):
            parts = line.split()
            if len(parts) > 1:
                out[STATUS] = parts[1]
            continue
        name, sep, value = line.partition(":")
        if sep:
            out[name.strip().lower()] = value.strip()
    return out


def probe(
    token: str, *, model: str = DEFAULT_PROBE_MODEL, timeout: float = 30.0
) -> dict[str, str] | None:
    """Response headers of a one-token request; None when the request never completed."""
    fake = os.environ.get("SEAT_BUDGET_FAKE_HEADERS")
    if fake:
        return _fake_headers(fake)
    request = urllib.request.Request(
        API_URL,
        data=_probe_body(model),
        method="POST",
        headers={
            "authorization": f"Bearer {token}",
            "anthropic-beta": "oauth-2025-04-20",
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return _headers_of(response.status, response.headers)
    except urllib.error.HTTPError as exc:
        # A 401/429 still carries the header block on some paths, and its status is itself
        # the answer — keep it rather than collapsing it into "probe failed".
        return _headers_of(exc.code, exc.headers)
    except (urllib.error.URLError, OSError):
        return None


def decide(
    headers: dict[str, str] | None, *, budget_max: float = DEFAULT_BUDGET_MAX
) -> Verdict:
    """Whether a trial may run right now — see the module docstring for the full table."""
    if headers is None:
        return Verdict(
            "go", "probe failed — budget gate is off for this poll", fail_open=True
        )
    status = headers.get(STATUS, "")
    if status == "429":
        # Model-specific caps 429 while both unified windows read as free, so their resets
        # say nothing about when this clears — re-probe at the 5h cadence instead.
        reset_5h = _num(headers.get(_HEADER.format(window="5h", field="reset")))
        retry_after = _num(headers.get("retry-after"))
        reset = reset_5h or (
            time.time() + retry_after if retry_after is not None else None
        )
        return Verdict(
            "wait",
            "probe returned HTTP 429 — the seat refuses this model right now",
            reset=int(reset) if reset else None,
            headroom=0.0,
            headroom_5h=0.0,
            reset_5h=int(reset_5h) if reset_5h else None,
        )
    if status != "200":
        return Verdict(
            "go",
            f"probe returned HTTP {status} — budget gate is off for this poll",
            fail_open=True,
        )

    used: dict[str, float] = {}
    for window in WINDOWS:
        value = _num(headers.get(_HEADER.format(window=window, field="utilization")))
        if value is None:
            return Verdict(
                "go",
                f"no {window} utilization header — rate-limit contract changed, "
                "budget gate is off",
                fail_open=True,
            )
        used[window] = value

    overage = _num(headers.get(_OVERAGE))
    reset_5h = _num(headers.get(_HEADER.format(window="5h", field="reset")))
    rolling = {
        "headroom_5h": 1.0 - used["5h"],
        "reset_5h": int(reset_5h) if reset_5h else None,
    }
    # The fuller window binds, and it is the one whose reset we must wait for: the other
    # resetting sooner would not free any quota.
    window = max(WINDOWS, key=lambda w: used[w])
    headroom = 1.0 - used[window]
    if used[window] >= budget_max:
        reset = _num(headers.get(_HEADER.format(window=window, field="reset")))
        return Verdict(
            "wait",
            f"{window} window at {used[window]:.2f} of {budget_max:.2f}",
            reset=int(reset) if reset else None,
            headroom=headroom,
            overage=overage,
            **rolling,
        )
    return Verdict(
        "go",
        f"{window} window at {used[window]:.2f} of {budget_max:.2f}",
        headroom=headroom,
        overage=overage,
        **rolling,
    )


#: A single 429 can be a momentary burst refusal caused by the trial's own traffic, and a
#: wait verdict mid-batch kills the running trial — so quota gets a second look first.
CONFIRM_DELAY_SEC = 15.0


def gate(
    token: str,
    *,
    model: str = DEFAULT_PROBE_MODEL,
    budget_max: float = DEFAULT_BUDGET_MAX,
) -> Verdict:
    """One probe → Verdict, with a confirming re-probe before any 429 wait."""
    headers = probe(token, model=model)
    if headers is not None and headers.get(STATUS) == "429":
        time.sleep(float(os.environ.get("SEAT_BUDGET_CONFIRM_DELAY", CONFIRM_DELAY_SEC)))
        headers = probe(token, model=model)
    return decide(headers, budget_max=budget_max)


def _field(value) -> str:
    return (
        "-"
        if value is None
        else (f"{value:.4f}" if isinstance(value, float) else str(value))
    )


if __name__ == "__main__":
    import argparse

    parser = argparse.ArgumentParser(
        description="Seat budget gate: prints "
        "'<go|wait> <reset|-> <headroom|-> <overage|-> <headroom_5h|-> <reset_5h|->'"
    )
    parser.add_argument(
        "--token-env",
        default="CC_TOKEN",
        help="env var holding the seat token (default: CC_TOKEN)",
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_PROBE_MODEL,
        help="model to probe with; pass the run's model so its own cap is visible "
        f"(default: {DEFAULT_PROBE_MODEL})",
    )
    parser.add_argument(
        "--budget-max",
        type=float,
        default=float(os.environ.get("BUDGET_MAX") or DEFAULT_BUDGET_MAX),
        help=f"stop at this share of the tighter window (default: "
        f"{DEFAULT_BUDGET_MAX})",
    )
    ns = parser.parse_args()

    token = os.environ.get(ns.token_env) or ""
    if not token and not os.environ.get("SEAT_BUDGET_FAKE_HEADERS"):
        verdict = Verdict(
            "go", f"${ns.token_env} is empty — budget gate is off", fail_open=True
        )
    else:
        verdict = gate(token, model=ns.model, budget_max=ns.budget_max)

    # Silent on the happy path: callers poll once a minute for hours and pass this stderr
    # through to the run log. A gate that is OFF must never be one of the silent cases.
    if verdict.fail_open:
        print(f"warning: {verdict.reason}", file=sys.stderr)
    elif verdict.action == "wait":
        print(verdict.reason, file=sys.stderr)
    print(
        f"{verdict.action} {_field(verdict.reset)} {_field(verdict.headroom)} "
        f"{_field(verdict.overage)} {_field(verdict.headroom_5h)} "
        f"{_field(verdict.reset_5h)}"
    )
