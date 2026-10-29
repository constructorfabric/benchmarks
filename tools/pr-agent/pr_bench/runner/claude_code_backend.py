"""PR-Agent backend that answers through Claude Code (`claude -p`) on subscription seats.

PR-Agent makes one call per chunk: system prompt + user prompt -> YAML text. Claude Code in
headless mode does exactly that with a seat's OAuth token (the value of `claude setup-token`),
so the seats are used by Claude Code itself.

Seats rotate in the order given by CLAUDE_SEATS (default `CC_TOKEN_2,CC_TOKEN_3,CC_TOKEN`, the
names of the env vars holding the tokens, read from tools/pr-agent/.env): a seat is used until
its tighter subscription window (5h or 7d) is 95% full, then the next one. The fill level comes
from `seat_budget.py`, vendored from spec-bench, which reads the rate-limit headers of an
8-token probe. That probe has to use a model the raw API serves to seat tokens (Opus 5.5 answers
it with 429 even when Claude Code serves it), and the windows are per seat, not per model, so
the probe model is the gate's default. A refusal of the review model itself surfaces from
`claude -p` and moves the call to the next seat. The 95% stop keeps the run off the overage
credits the seats' human users rely on.

Two modes:
  bare     no tools at all: the same single-shot call the API backends make, so the review
           is comparable with glm/deepseek on the same prompt.
  agentic  Read/Grep/Glob allowed in the PR worktree: the model can open whole files instead
           of seeing only the diff. A different tool, reported as such.

`python -m pr_bench.runner.claude_code_backend --print-cmd` shows the command line; `--seats` prints each
seat's verdict (one 8-token probe per seat, no review call).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import sys
import time
from dataclasses import dataclass
from pathlib import Path

from pr_bench.runner import seat_budget

AGENTIC_TOOLS = "Read,Grep,Glob"
DEFAULT_SEATS = "CC_TOKEN_2,CC_TOKEN_3,CC_TOKEN"
GATE_TTL_S = 120
RATE_LIMIT_BACKOFF_S = 900
SEAT_WAIT_MAX_S = int(os.environ.get("SEAT_WAIT_MAX", "21600"))
RATE_LIMIT_MARKERS = ("usage limit", "rate limit", "resets at", "429", "overloaded")
AUTH_MARKERS = ("invalid api key", "authentication", "oauth token", "401")


@dataclass(frozen=True)
class ClaudeCodeCall:
    model: str
    system: str
    user: str
    agentic: bool
    cwd: Path

    def argv(self) -> list[str]:
        cmd = [
            shutil.which("claude") or "claude",
            "-p",
            "--model", self.model,
            "--output-format", "json",
            "--system-prompt", self.system,
            # no CLAUDE.md, no user settings, no project memory: the prompt is the whole context
            "--setting-sources", "",
        ]
        if self.agentic:
            cmd += ["--tools", AGENTIC_TOOLS, "--allowed-tools", AGENTIC_TOOLS, "--permission-mode", "dontAsk",
                    "--add-dir", str(self.cwd)]
        else:
            cmd += ["--tools", ""]
        return cmd


class SeatPool:
    """Picks the first seat in order that the budget gate allows; remembers refusals."""

    def __init__(self, names: list[str]):
        self.names = [n for n in names if os.environ.get(n)]
        if not self.names: raise RuntimeError(f"no seat tokens set among {names} (tools/pr-agent/.env)")
        self.blocked_until: dict[str, float] = {}
        self.verdicts: dict[str, tuple[float, seat_budget.Verdict]] = {}

    def verdict(self, name: str) -> seat_budget.Verdict:
        cached = self.verdicts.get(name)
        if cached and time.time() - cached[0] < GATE_TTL_S: return cached[1]
        v = seat_budget.gate(os.environ[name])
        self.verdicts[name] = (time.time(), v)
        return v

    def block(self, name: str, until: float) -> None:
        self.blocked_until[name] = until
        self.verdicts.pop(name, None)

    async def acquire(self) -> str:
        started = time.time()
        while True:
            soonest = None
            for name in self.names:
                if time.time() < self.blocked_until.get(name, 0):
                    soonest = min(filter(None, (soonest, self.blocked_until[name])))
                    continue
                v = await asyncio.to_thread(self.verdict, name)
                if v.action == "go": return name
                until = float(v.reset or time.time() + RATE_LIMIT_BACKOFF_S)
                self.block(name, until)
                soonest = min(filter(None, (soonest, until)))
            if time.time() - started > SEAT_WAIT_MAX_S:
                raise RuntimeError(f"all seats {self.names} stayed unavailable for {SEAT_WAIT_MAX_S}s")
            sleep_s = max(60.0, min((soonest or time.time()) - time.time(), RATE_LIMIT_BACKOFF_S))
            print(f"seats: none available, sleeping {sleep_s:.0f}s", file=sys.stderr, flush=True)
            await asyncio.sleep(sleep_s)


_pool: SeatPool | None = None


def seat_pool() -> SeatPool:
    global _pool
    if _pool is None: _pool = SeatPool(os.environ.get("CLAUDE_SEATS", DEFAULT_SEATS).split(","))
    return _pool


def env_for_seat(token: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items()
           if k not in ("ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL")}
    env["CLAUDE_CODE_OAUTH_TOKEN"] = token
    env["CLAUDE_FORCE_OAUTH"] = "1"
    return env


def parse_result(stdout: str) -> tuple[str, dict]:
    """`--output-format json` prints one object with `result` and usage; fall back to raw text."""
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError:
        return stdout, {}
    if isinstance(data, list):
        data = next((d for d in reversed(data) if isinstance(d, dict) and d.get("type") == "result"), data[-1] if data else {})
    if not isinstance(data, dict): return stdout, {}
    text = data.get("result") if isinstance(data.get("result"), str) else stdout
    meta = {k: data.get(k) for k in ("usage", "total_cost_usd", "num_turns", "duration_ms", "is_error", "stop_reason") if k in data}
    return text, meta


def classify_failure(text: str) -> str:
    low = text.lower()
    if any(m in low for m in RATE_LIMIT_MARKERS): return "rate_limited"
    if any(m in low for m in AUTH_MARKERS): return "auth_failed"
    return "other"


async def _run_once(call: ClaudeCodeCall, token: str, timeout_s: float) -> tuple[int, str, str]:
    proc = await asyncio.create_subprocess_exec(
        *call.argv(), cwd=call.cwd, env=env_for_seat(token),
        stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        out, err = await asyncio.wait_for(proc.communicate(call.user.encode()), timeout=timeout_s)
    except TimeoutError:
        proc.kill()
        raise RuntimeError(f"claude -p timed out after {timeout_s:.0f}s") from None
    return proc.returncode, out.decode(errors="replace"), err.decode(errors="replace")


async def run_claude(call: ClaudeCodeCall, timeout_s: float) -> tuple[str, dict]:
    pool = seat_pool()
    for _ in range(3 * len(pool.names)):
        seat = await pool.acquire()
        code, out, err = await _run_once(call, os.environ[seat], timeout_s)
        text, meta = parse_result(out)
        failed = code != 0 or meta.get("is_error")
        if not failed:
            meta["seat"] = seat
            return text, meta
        kind = classify_failure(f"{text}\n{err}")
        if kind == "other":
            raise RuntimeError(f"claude -p on {seat} failed (exit {code}): {(err or text)[-800:]}")
        print(f"seats: {seat} {kind}, moving on: {(err or text)[-200:]!r}", file=sys.stderr, flush=True)
        pool.block(seat, time.time() + (RATE_LIMIT_BACKOFF_S if kind == "rate_limited" else 10 ** 9))
    raise RuntimeError("claude -p failed on every seat")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--print-cmd", action="store_true", help="print the argv for a sample call and exit")
    parser.add_argument("--seats", action="store_true", help="print each seat's budget verdict (8-token probes)")
    parser.add_argument("--model", default="claude-opus-5-5")
    parser.add_argument("--agentic", action="store_true")
    args = parser.parse_args()
    if args.seats:
        for name in os.environ.get("CLAUDE_SEATS", DEFAULT_SEATS).split(","):
            if not os.environ.get(name): print(f"{name}: not set"); continue
            v = seat_budget.gate(os.environ[name])
            print(f"{name}: {v.action} headroom={v.headroom} headroom_5h={v.headroom_5h} reset={v.reset} ({v.reason})")
        return
    call = ClaudeCodeCall(args.model, "<system prompt>", "<user prompt on stdin>", args.agentic, Path.cwd())
    if args.print_cmd:
        print(" ".join(f"'{a}'" if " " in a or a == "" else a for a in call.argv()))
        return
    parser.error("nothing to do without --print-cmd or --seats; the backend is driven by run_review.py")


if __name__ == "__main__":
    main()
