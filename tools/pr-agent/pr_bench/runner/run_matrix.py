"""Run the review matrix: PRs x models x variants x runs, with per-model concurrency.

Each job is one `run_review.py` subprocess inside a git worktree checked out at the
PR head (`origin/pull/<N>/head`). Jobs whose out dir already holds result.json are
skipped, so an interrupted matrix is resumed by running the same command again.

Usage:
  python -m pr_bench.runner.run_matrix --prs 73 91 102 --models glm-5.3-flash deepseek-v4.1-flash \
      --variants per-module single --runs 1 --out out/pilot
Environment: LITELLM_ROUTER_V1, LITELLM_KEY, optional PR_AGENT_PYTHON (interpreter with pr-agent).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path

from pr_bench.paths import INSTRUCTIONS, OUT, REPO_ROOT, WORKTREES

MODULES = ("errors", "security", "async", "design", "tests", "toolkit", "architecture")
# Jobs per model. Each job also runs up to PR_AGENT_MAX_CONCURRENT_CALLS chunk calls (run_review.py,
# default 2), so in-flight requests per model = jobs x calls. Measured on the shared nodes:
# deepseek-v4.1-flash answers a 114k-token chunk in ~57 s alone and 240-940 s with ~48 in flight;
# glm-5.3-flash (two nodes) went from ~145 s to 400-1450 s. Router calls are capped machine-wide
# anyway (router_slots.py), so more jobs only queue longer.
DEFAULT_CONCURRENCY = {"glm-5.3-flash": 2, "deepseek-v4.1-flash": 2, "claude-code/claude-opus-5-5": 2}


@dataclass(frozen=True)
class Job:
    pr: int
    model: str
    variant: str
    module: str
    run: int

    @property
    def rel_out(self) -> Path:
        return Path(f"pr-{self.pr}") / self.model.replace("/", "_") / f"run-{self.run}" / self.variant / self.module

    @property
    def instructions(self) -> Path:
        name = "all.md" if self.variant == "single" else f"{self.module}.md"
        return INSTRUCTIONS / self.variant / name


def build_jobs(prs: list[int], models: list[str], variants: list[str], runs: int, first_run: int = 1) -> list[Job]:
    jobs = []
    for pr in prs:
        for model in models:
            for run in range(first_run, runs + 1):
                for variant in variants:
                    modules = MODULES if variant == "per-module" else ("all",)
                    jobs.extend(Job(pr, model, variant, m, run) for m in modules)
    return jobs


def ensure_worktree(pr: int, worktrees: Path) -> Path:
    path = worktrees / f"pr-{pr}"
    if path.exists():
        return path
    subprocess.run(["git", "fetch", "-q", "origin", f"pull/{pr}/head:pr-{pr}"], cwd=REPO_ROOT, check=True)
    subprocess.run(["git", "worktree", "add", "-q", "--detach", str(path), f"pr-{pr}"], cwd=REPO_ROOT, check=True)
    return path


async def run_job(job: Job, out_root: Path, worktree: Path, python: str, semaphore: asyncio.Semaphore,
                  log: Path, calls_per_job: int, agentic: bool = False) -> dict:
    out_dir = out_root / job.rel_out
    if (out_dir / "result.json").exists():
        return {**asdict(job), "status": "skipped"}
    out_dir.mkdir(parents=True, exist_ok=True)
    cmd = [python, "-I", "-m", "pr_bench.runner.run_review", "--model", job.model, "--instructions", str(job.instructions),
           "--out", str(out_dir), "--pr", str(job.pr), "--variant", job.variant, "--module", job.module,
           "--run", str(job.run)]
    if agentic and job.model.startswith("claude-code/"): cmd.append("--agentic")
    env = {**os.environ, "PR_AGENT_MAX_CONCURRENT_CALLS": str(calls_per_job)}
    async with semaphore:
        started = time.time()
        with (out_dir / "stderr.log").open("w") as err:
            proc = await asyncio.create_subprocess_exec(*cmd, cwd=worktree, stdout=err, stderr=err, env=env)
            code = await proc.wait()
    record = {**asdict(job), "status": "ok" if code == 0 else f"exit {code}", "seconds": round(time.time() - started, 1)}
    with log.open("a") as f:
        f.write(json.dumps(record) + "\n")
    print(json.dumps(record), flush=True)
    return record


async def run_all(args: argparse.Namespace) -> list[dict]:
    jobs = build_jobs(args.prs, args.models, args.variants, args.runs, args.first_run)
    worktrees = {pr: ensure_worktree(pr, args.worktrees) for pr in args.prs}
    semaphores = {m: asyncio.Semaphore(args.concurrency.get(m, 2)) for m in args.models}
    log = args.out / "matrix.jsonl"
    args.out.mkdir(parents=True, exist_ok=True)
    tasks = [run_job(j, args.out, worktrees[j.pr], args.python, semaphores[j.model], log, args.calls_per_job,
                     args.agentic) for j in jobs]
    return await asyncio.gather(*tasks)


def parse_concurrency(values: list[str]) -> dict[str, int]:
    pairs = dict(DEFAULT_CONCURRENCY)
    for value in values:
        model, n = value.split("=")
        pairs[model] = int(n)
    return pairs


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--prs", type=int, nargs="+", required=True)
    parser.add_argument("--models", nargs="+", default=list(DEFAULT_CONCURRENCY))
    parser.add_argument("--variants", nargs="+", default=["per-module", "single"], choices=["per-module", "single"])
    parser.add_argument("--runs", type=int, default=1, help="last run index (inclusive)")
    parser.add_argument("--first-run", type=int, default=1, help="first run index, to add runs to a set already in progress")
    parser.add_argument("--out", type=Path, default=OUT / "matrix")
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--concurrency", nargs="*", default=[], help="model=N overrides (jobs per model)")
    parser.add_argument("--calls-per-job", type=int, default=2, help="in-flight chunk calls per job")
    parser.add_argument("--agentic", action="store_true", help="claude-code/* jobs may Read/Grep/Glob the worktree")
    parser.add_argument("--python", default=os.environ.get("PR_AGENT_PYTHON", sys.executable))
    args = parser.parse_args()
    args.concurrency = parse_concurrency(args.concurrency)
    args.out, args.worktrees = args.out.resolve(), args.worktrees.resolve()
    if any(not m.startswith("claude-code/") for m in args.models):
        for key in ("LITELLM_ROUTER_V1", "LITELLM_KEY"):
            if not os.environ.get(key): parser.error(f"{key} is not set")
    results = asyncio.run(run_all(args))
    failed = [r for r in results if r["status"] not in ("ok", "skipped")]
    print(f"done: {len(results)} jobs, {len(failed)} failed")
    if failed: sys.exit(1)


if __name__ == "__main__":
    main()
