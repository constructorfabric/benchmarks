"""LLM calls of the analysis scripts: one model backend per judge, a cached task loop, JSON parsing.

Backends:
  opus        Opus through the Claude Code seats (bare, or agentic with Read/Grep/Glob in `cwd`)
  glm         glm-5.3-flash through the router, a plain chat call
  glm-agent   Claude Code pointed at the router with glm-5.3-flash: the same tools as agentic Opus

Router calls take a machine-wide slot (router_slots.py) and wait out a 429, like the reviews do.
Every script keeps its own task key and record; `run_cached` appends one JSON line per answered
task to the script's cache, so a rerun only calls what is missing.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import sys
import time
from collections.abc import Awaitable, Callable, Iterable
from pathlib import Path

from pr_bench.paths import ROOT
from pr_bench.runner.claude_code_backend import ClaudeCodeCall, run_claude
from pr_bench.runner.router_slots import is_rate_limited, router_slot

OPUS = "claude-opus-5-5"
GLM = "glm-5.3-flash"
RATE_LIMIT_RETRIES = 20
RATE_LIMIT_WAIT_S = 30

async def _glm_chat(system: str, user: str, timeout_s: float) -> str:
    import litellm  # noqa: PLC0415  slow import, only the glm judge needs it
    for attempt in range(RATE_LIMIT_RETRIES):
        async with router_slot():
            try:
                resp = await litellm.acompletion(
                    model=f"openai/{GLM}", api_base=os.environ["LITELLM_ROUTER_V1"], api_key=os.environ["LITELLM_KEY"],
                    timeout=timeout_s, max_tokens=8000,
                    messages=[{"role": "system", "content": system}, {"role": "user", "content": user}])
                return resp.choices[0].message.content or ""
            except Exception as e:
                if not is_rate_limited(e) or attempt == RATE_LIMIT_RETRIES - 1: raise
        await asyncio.sleep(RATE_LIMIT_WAIT_S)
    raise RuntimeError("unreachable")


async def _glm_agent(system: str, user: str, cwd: Path, timeout_s: float) -> str:
    base = os.environ["LITELLM_ROUTER_V1"].removesuffix("/v1")
    env = {k: v for k, v in os.environ.items()
           if k not in ("CLAUDE_CODE_OAUTH_TOKEN", "CLAUDE_FORCE_OAUTH", "ANTHROPIC_API_KEY")}
    env.update(ANTHROPIC_BASE_URL=base, ANTHROPIC_AUTH_TOKEN=os.environ["LITELLM_KEY"], ANTHROPIC_MODEL=GLM,
               **{k: GLM for k in ("ANTHROPIC_DEFAULT_HAIKU_MODEL", "ANTHROPIC_DEFAULT_SONNET_MODEL",
                                   "ANTHROPIC_DEFAULT_OPUS_MODEL", "ANTHROPIC_SMALL_FAST_MODEL")})
    call = ClaudeCodeCall(GLM, system, user, True, cwd)
    for _ in range(RATE_LIMIT_RETRIES):
        async with router_slot():
            proc = await asyncio.create_subprocess_exec(*call.argv(), cwd=cwd, env=env, stdin=asyncio.subprocess.PIPE,
                                                        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            try:
                out, _ = await asyncio.wait_for(proc.communicate(user.encode()), timeout=timeout_s)
            except TimeoutError:
                proc.kill()
                raise RuntimeError("glm session timed out") from None
        data = json.loads(out.decode(errors="replace") or "{}")
        text = str(data.get("result") or "")
        if data.get("is_error") and is_rate_limited(text):
            await asyncio.sleep(RATE_LIMIT_WAIT_S)
            continue
        if data.get("is_error"): raise RuntimeError(text[:300])
        return text
    raise RuntimeError(f"glm session rate-limited {RATE_LIMIT_RETRIES} times")


async def ask(backend: str, system: str, user: str, *, cwd: Path = ROOT, agentic: bool = False,
              timeout_s: float = 900) -> str:
    if backend == "opus":
        text, _ = await run_claude(ClaudeCodeCall(OPUS, system, user, agentic, cwd), timeout_s)
        return text
    if backend == "glm": return await _glm_chat(system, user, timeout_s)
    if backend == "glm-agent": return await _glm_agent(system, user, cwd, timeout_s)
    raise ValueError(f"unknown backend {backend}")


async def run_cached[T](tasks: list[T], call: Callable[[T], Awaitable[str]], record: Callable[[T, str], dict | None],
                     cache: Path, concurrency: int, label: str, every: int = 50) -> None:
    """Call each task (two attempts), turn the answer into a cache record, append it to `cache`.
    `record` returns None for an answer it cannot parse; such a task is counted and retried once."""
    sem, lock, started, stats = asyncio.Semaphore(concurrency), asyncio.Lock(), time.time(), {"ok": 0, "bad": 0}

    async def one(task: T) -> None:
        async with sem:
            rec = None
            for _ in range(2):
                try:
                    rec = record(task, await call(task))
                except Exception as e:
                    print(f"  {label}: {str(e)[:200]}", file=sys.stderr, flush=True)
                if rec: break
        stats["ok" if rec else "bad"] += 1
        async with lock:
            if rec:
                with cache.open("a") as fh: fh.write(json.dumps(rec) + "\n")
            n = stats["ok"] + stats["bad"]
            if n % every == 0: print(f"  {n}/{len(tasks)} in {time.time() - started:.0f}s, unparsed {stats['bad']}", flush=True)

    await asyncio.gather(*(one(t) for t in tasks))
    print(f"{label}: done, {stats['ok']} answered, {stats['bad']} unparsed after retry", flush=True)


def load_cache(cache: Path) -> list[dict]:
    return [json.loads(line) for line in cache.open()] if cache.exists() else []


def last_json[T](text: str, accept: Callable[[dict], T | None], pattern: str = r"\{[^{}]*\}") -> T | None:
    """The last JSON object in the answer that `accept` takes; models may write reasoning before it."""
    for m in reversed(list(re.finditer(pattern, text, re.S))):
        try:
            data = json.loads(m.group(0))
        except json.JSONDecodeError:
            continue
        if isinstance(data, dict) and (out := accept(data)) is not None: return out
    return None


NESTED_OBJECT = r"\{[^{}]*\{[^{}]*\}[^{}]*\}"


def parse_labels(text: str, n: int, allowed: Iterable[str], prefix: str = "B") -> dict[str, str] | None:
    """{"labels": {"B1": ..., "Bn": ...}} with every label in `allowed`."""
    allowed = set(allowed)

    def accept(data: dict) -> dict[str, str] | None:
        labels = data.get("labels", {})
        out = {f"{prefix}{i}": str(labels.get(f"{prefix}{i}", "")).lower() for i in range(1, n + 1)}
        return out if all(v in allowed for v in out.values()) else None

    return last_json(text, accept, NESTED_OBJECT)
