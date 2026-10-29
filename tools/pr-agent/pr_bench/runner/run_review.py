"""Run one PR-Agent /review of a local branch against `main` and dump everything.

Reads the router from the environment: LITELLM_ROUTER_V1 (…/v1), LITELLM_KEY.
Writes into --out:
  review.md        PR-Agent's rendered review
  result.json      merged review dict, coverage, timings, per-call stats
  calls/NNN.*      raw model output, the text actually parsed, the user prompt

Usage (from inside the checked-out PR worktree; run_matrix.py starts it):
  python -m pr_bench.runner.run_review --model glm-5.3-flash --instructions ../instructions/per-module/errors.md --out out/...
"""

from __future__ import annotations

import argparse
import asyncio
import itertools
import json
import os
import re
import sys
import time
import tomllib
from pathlib import Path

from pr_agent.algo.ai_handlers.litellm_ai_handler import LiteLLMAIHandler
from pr_agent.algo.utils import load_yaml
from pr_agent.config_loader import get_settings
from pr_agent.git_providers.local_git_provider import LocalGitProvider
from pr_agent.tools.pr_reviewer import PRReviewer

from pr_bench.paths import PR_AGENT_TOML
from pr_bench.runner.claude_code_backend import ClaudeCodeCall, run_claude
from pr_bench.runner.router_slots import is_rate_limited, router_slot

CLAUDE_CODE_PREFIX = "claude-code/"
RATE_LIMIT_RETRIES = 30
RATE_LIMIT_WAIT_S = 30
RUST_SUFFIXES = (".rs",)
MANIFEST_NAMES = {"Cargo.toml", "Cargo.lock", "deny.toml", "audit.toml", "config.toml", "clippy.toml", "rust-toolchain.toml"}
YAML_FENCE = "```yaml"


def is_reviewable(path: str) -> bool:
    name = path.rsplit("/", 1)[-1]
    return path.endswith(RUST_SUFFIXES) or name in MANIFEST_NAMES


def take_last_yaml_block(text: str) -> str:
    """glm-5.3-flash leaks its reasoning into the answer when the diff contains a literal
    `</think>`; the final answer still comes last and starts with a top-level `review:` line,
    fenced or not. Placeholder blocks inside the reasoning (`review:\\n  ...`) come earlier,
    so the last occurrence is the one. A clean answer is returned unchanged in substance."""
    starts = [m.start() for m in re.finditer(r"(?m)^review:[ \t]*$", text)]
    if not starts: return text
    block = text[starts[-1]:].rstrip()
    return block.removesuffix("```").rstrip()


def parses_as_review(text: str) -> bool:
    try:
        data = load_yaml(text.strip(), keys_fix_yaml=[])
    except Exception:
        return False
    return isinstance(data, dict) and isinstance(data.get("review"), dict)


class RustOnlyLocalGitProvider(LocalGitProvider):
    """`ignore.regex` is not implemented for the local platform, so filter here. The
    toolkit-pr-review skill is Rust-only: .rs files plus the manifest/lint configs."""

    skipped_files: list[str] = []

    def get_diff_files(self):
        files = super().get_diff_files()
        kept = [f for f in files if is_reviewable(f.filename)]
        self.skipped_files = sorted(f.filename for f in files if not is_reviewable(f.filename))
        return kept


class RecordingAIHandler(LiteLLMAIHandler):
    """PR-Agent fires every chunk of a review at once (`asyncio.gather`, no limit); a 16-chunk
    PR times several matrix jobs saturated the shared nodes. The semaphore caps in-flight
    calls per process; run_matrix.py caps processes per model."""

    calls_dir: Path
    calls: list[dict] = []
    counter = itertools.count()
    semaphore = asyncio.Semaphore(int(os.environ.get("PR_AGENT_MAX_CONCURRENT_CALLS", "2")))
    # set by configure() for the Claude Code backend; None means litellm against the router
    claude_code_model: str | None = None
    agentic = False
    cwd = Path.cwd()
    timeout_s = 1800.0

    async def _complete(self, model: str, system: str, user: str, temperature: float, img_path):
        if self.claude_code_model is None:
            # The router key allows 2 parallel requests and the nodes are at capacity: take a
            # machine-wide slot, and wait out a 429 instead of letting PR-Agent drop the chunk.
            for attempt in range(RATE_LIMIT_RETRIES):
                async with router_slot():
                    try:
                        response, finish_reason = await super().chat_completion(model, system, user, temperature, img_path)
                        return response, finish_reason, {"rate_limit_retries": attempt}
                    except Exception as e:
                        if not is_rate_limited(e) or attempt == RATE_LIMIT_RETRIES - 1: raise
                await asyncio.sleep(RATE_LIMIT_WAIT_S)
        response, meta = await run_claude(ClaudeCodeCall(self.claude_code_model, system, user, self.agentic, self.cwd), self.timeout_s)
        return response, meta.get("stop_reason") or "stop", meta

    async def chat_completion(self, model: str, system: str, user: str, temperature: float = 0.2, img_path: str = None):
        index = next(self.counter)
        queued = time.time()
        async with self.semaphore:
            started = time.time()
            response, finish_reason, meta = await self._complete(model, system, user, temperature, img_path)
        fixed = take_last_yaml_block(response)
        record = {
            "index": index,
            "backend_meta": meta,
            "seconds": round(time.time() - started, 1),
            "queued_seconds": round(started - queued, 1),
            "finish_reason": finish_reason,
            "prompt_chars": len(system) + len(user),
            "prompt_has_think_tag": "</think>" in user,
            "response_chars": len(response),
            "yaml_fences": response.count(YAML_FENCE),
            "raw_parses": parses_as_review(response),
            "fixed_parses": parses_as_review(fixed),
            "reasoning_leaked": not response.lstrip().startswith(YAML_FENCE) and response.count(YAML_FENCE) > 0,
        }
        self.calls.append(record)
        returned = fixed if record["fixed_parses"] or not record["raw_parses"] else response
        stem = self.calls_dir / f"{index:03d}"
        stem.with_suffix(".raw.txt").write_text(response)
        stem.with_suffix(".parsed.txt").write_text(returned)
        stem.with_suffix(".prompt.txt").write_text(user)
        return returned, finish_reason


def apply_toml(settings, path: Path) -> None:
    """PR-Agent's merge loader ignores `settings.load_file`, so set every key explicitly."""
    for section, values in tomllib.loads(path.read_text()).items():
        for key, value in values.items():
            settings.set(f"{section}.{key}", value)


def configure(args: argparse.Namespace) -> None:
    settings = get_settings()
    apply_toml(settings, PR_AGENT_TOML)
    if args.model.startswith(CLAUDE_CODE_PREFIX):
        # PR-Agent still needs a model name for token accounting; the call itself goes to `claude -p`.
        RecordingAIHandler.claude_code_model = args.model.removeprefix(CLAUDE_CODE_PREFIX)
        RecordingAIHandler.agentic = args.agentic
        RecordingAIHandler.cwd = Path.cwd()
        RecordingAIHandler.timeout_s = float(settings.config.ai_timeout)
        settings.set("config.model", f"openai/{RecordingAIHandler.claude_code_model}")
        settings.set("openai.key", os.environ.get("LITELLM_KEY", "unused"))
        settings.set("openai.api_base", os.environ.get("LITELLM_ROUTER_V1", "http://unused.invalid/v1"))
    else:
        settings.set("config.model", f"openai/{args.model}")
        settings.set("openai.key", os.environ["LITELLM_KEY"])
        settings.set("openai.api_base", os.environ["LITELLM_ROUTER_V1"])
    settings.set("local.review_path", str(args.out / "review.md"))
    settings.set("pr_reviewer.extra_instructions", args.instructions.read_text())
    if int(settings.pr_reviewer.num_max_findings) == 0:
        # PR-Agent renders "(0-{{ num_max_findings }} issues)"; 0 would read as "0-0". Drop the cap.
        system = settings.pr_review_prompt.system.replace(
            "A concise list (0-{{ num_max_findings }} issues) of", "A list of every one of the")
        settings.set("pr_review_prompt.system", system)
    if args.max_model_tokens:
        settings.set("config.max_model_tokens", args.max_model_tokens)
        settings.set("config.custom_model_max_tokens", args.max_model_tokens)


def count_check_ids(review: dict) -> dict[str, int]:
    counts: dict[str, int] = {}
    for issue in review.get("key_issues_to_review") or []:
        header = str(issue.get("issue_header", "")) if isinstance(issue, dict) else str(issue)
        match = re.match(r"\s*([A-Z]+(?:-[A-Z]+)*-\d+)", header)
        key = match.group(1) if match else "UNMAPPED"
        counts[key] = counts.get(key, 0) + 1
    return counts


async def review(args: argparse.Namespace) -> dict:
    RecordingAIHandler.calls_dir = args.out / "calls"
    RecordingAIHandler.calls_dir.mkdir(parents=True, exist_ok=True)
    RecordingAIHandler.calls = []
    RecordingAIHandler.counter = itertools.count()
    started = time.time()
    reviewer = PRReviewer("main", ai_handler=RecordingAIHandler)
    provider: RustOnlyLocalGitProvider = reviewer.git_provider  # type: ignore[assignment]
    await reviewer.run()
    data = reviewer.prediction_data
    if data is None and reviewer.prediction:
        data = load_yaml(reviewer.prediction.strip(), keys_fix_yaml=[])
    merged = (data or {}).get("review", {}) if isinstance(data, dict) else {}
    return {
        "model": args.model,
        "backend": "claude-code" if RecordingAIHandler.claude_code_model else "litellm",
        "agentic": bool(RecordingAIHandler.claude_code_model) and args.agentic,
        "variant": args.variant,
        "module": args.module,
        "run": args.run,
        "pr": args.pr,
        "head_branch": provider.head_branch_name,
        "seconds_total": round(time.time() - started, 1),
        "calls": RecordingAIHandler.calls,
        "chunks": getattr(reviewer, "review_chunk_count", len(RecordingAIHandler.calls)),
        "failed_chunks": getattr(reviewer, "review_failed_chunk_count", 0),
        "files_reviewed": len(provider.get_diff_files()),
        "files_skipped_not_rust": provider.skipped_files,
        "files_omitted_by_budget": list(reviewer.remaining_files_list or []),
        "files_partially_included": list(getattr(reviewer, "partial_files_list", []) or []),
        "review_parsed": isinstance(data, dict),
        "findings_count": len(merged.get("key_issues_to_review") or []),
        "check_id_counts": count_check_ids(merged),
        "review": merged,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--model", required=True)
    parser.add_argument("--instructions", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--pr", type=int, default=0)
    parser.add_argument("--variant", default="")
    parser.add_argument("--module", default="")
    parser.add_argument("--run", type=int, default=1)
    parser.add_argument("--max-model-tokens", type=int, default=0)
    parser.add_argument("--agentic", action="store_true",
                        help="claude-code/* only: allow Read/Grep/Glob in the worktree instead of a bare single-shot call")
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    configure(args)
    from pr_agent.git_providers import _GIT_PROVIDERS  # noqa: PLC0415  registered lazily by PR-Agent

    _GIT_PROVIDERS["local"] = RustOnlyLocalGitProvider
    result = asyncio.run(review(args))
    is_complete = result["review_parsed"] and not result["failed_chunks"] and not (
        result["findings_count"] == 0 and result["files_omitted_by_budget"])
    # PR-Agent swallows review errors and drops failed chunks; an incomplete review is kept aside so a rerun redoes it
    (args.out / ("result.json" if is_complete else "result.partial.json")).write_text(json.dumps(result, indent=1, ensure_ascii=False))
    leaked = sum(c["reasoning_leaked"] for c in result["calls"])
    print(f"RESULT pr={args.pr} model={args.model} {args.variant}/{args.module} run={args.run} "
          f"calls={len(result['calls'])} leaked={leaked} findings={result['findings_count']} "
          f"omitted={len(result['files_omitted_by_budget'])} {result['seconds_total']}s")
    if not is_complete: sys.exit(f"incomplete review: failed_chunks={result['failed_chunks']}, parsed={result['review_parsed']}")


if __name__ == "__main__":
    main()
