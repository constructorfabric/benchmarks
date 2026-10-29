"""Where in each PR is the cause of its failed hidden tests? Agentic Opus does the fault localization.

Input: out/tests/failed.json (failed_tests.py). Failures of one PR are grouped into clusters that
very likely share one cause: same test class and test function (parametrizations merged) and the
same normalised first assertion line (ids and numbers stripped). Each cluster gets one Claude Code
call in the PR's worktree with Read/Grep/Glob: the test's source from spec-bench, the failure
output, and the gear's own PRD/DESIGN under gears/mini-chat/docs. It returns the root-cause
location(s) and how visible the defect is to a reviewer who sees only the diff:

  code_evident     the code contradicts itself or is plainly wrong (panic, lost error, race,
                   wrong branch); a careful reviewer could see it from the diff alone
  spec_dependent   the code is consistent but differs from the documented contract (status
                   code, header format, error type, field name); needs the PRD/DESIGN to see
  missing_feature  the behaviour is not implemented at all; little to anchor a finding to

Results are cached in out/tests/localized.jsonl, one line per (pr, cluster); a rerun only calls
what is missing. Runs on the Claude Code seats (claude_code_backend.py, same rotation). The test
sources come from the spec-bench checkout (--suite, or SPEC_BENCH_SUITE).

Usage:
  python -m pr_bench.analysis.localize_failures --dry-run          # clusters per PR, no calls
  python -m pr_bench.analysis.localize_failures --limit 3          # smoke
  python -m pr_bench.analysis.localize_failures                    # everything
"""

from __future__ import annotations

import argparse
import ast
import asyncio
import json
import os
import re
from collections import defaultdict
from pathlib import Path

from pr_bench.analysis.llm import ask, last_json, load_cache, run_cached
from pr_bench.paths import OUT, WORKTREES

DEFAULT_SUITE = Path.home() / "00_CONSTRUCTOR_LLM/spec-bench/datasets/spec-bench/B9-minichat/tests/suite"
CONCURRENCY = 4
VISIBILITY = ("code_evident", "spec_dependent", "missing_feature")

SYSTEM = """You are debugging a Rust service against its black-box end-to-end tests.
You are inside the repository of one implementation of the mini-chat gear. The gear lives in
gears/mini-chat/ (code in gears/mini-chat/mini-chat/src/, docs in gears/mini-chat/docs/: PRD.md,
DESIGN.md, ADR/, acceptance-criteria.md). An e2e test from the hidden suite failed against this
implementation. Find the code in THIS repository that causes the failure: read the test, read the
failure, then use Grep/Read to locate the handler, service and repository code involved.

Report the root cause as precisely as you can: the file(s) and line range(s) whose change would make
the test pass. Classify how visible the defect is to a code reviewer who sees only the code diff:
- code_evident: the code is self-inconsistent or plainly wrong (panic, lost error, race, wrong branch)
- spec_dependent: the code is consistent but differs from the documented contract (status code,
  header, error type, field shape) - you need PRD/DESIGN/acceptance criteria to know it is wrong
- missing_feature: the behaviour is not implemented at all

Answer with one JSON object only, no prose around it:
{"cause": "<one sentence>", "visibility": "code_evident|spec_dependent|missing_feature",
 "confidence": "high|medium|low",
 "locations": [{"file": "<repo-relative path>", "start_line": <int>, "end_line": <int>, "role": "root_cause|related"}]}"""


def normalise(line: str) -> str:
    line = re.sub(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}", "<id>", line)
    line = re.sub(r"\b[0-9A-Za-z]{20,}\b", "<id>", line)
    return re.sub(r"\d+", "<n>", line)[:160]


def first_assert(detail: str) -> str:
    return next((line.strip() for line in detail.splitlines() if line.strip().startswith("E ")), "(no assertion text)")


def clusters_for(fails: list[dict]) -> list[dict]:
    groups: dict[str, list[dict]] = defaultdict(list)
    for f in fails:
        base = re.sub(r"\[.*\]$", "", f["test"])
        groups[f"{base} || {normalise(first_assert(f.get('detail', '')))}"].append(f)
    return [{"key": k, "tests": [f["test"] for f in fs], "detail": next((f["detail"] for f in fs if f.get("detail")), "")}
            for k, fs in sorted(groups.items())]


def test_source(test_id: str, suite: Path, max_lines: int = 80) -> str:
    module, cls, name = (test_id.split("::") + ["", ""])[:3]
    name = re.sub(r"\[.*\]$", "", name)
    path = suite / f"{module}.py"
    if not path.exists(): return "(test source not found)"
    src = path.read_text()
    for node in ast.walk(ast.parse(src)):
        if isinstance(node, ast.ClassDef) and node.name == cls:
            for fn in node.body:
                if isinstance(fn, (ast.FunctionDef, ast.AsyncFunctionDef)) and fn.name == name:
                    start = min([d.lineno for d in fn.decorator_list] + [fn.lineno]) - 1
                    lines = src.splitlines()[start:fn.end_lineno]
                    return f"# {path.name}:{start + 1}\n" + "\n".join(lines[:max_lines])
    return "(test function not found)"


def prompt(pr: int, cluster: dict, suite: Path) -> str:
    tests = cluster["tests"]
    return "\n".join([
        f"Implementation: pull request #{pr}. Failed test(s) with this cause ({len(tests)}):",
        *[f"- {t}" for t in tests[:12]], "",
        "Test source (spec-bench hidden suite, testing/e2e/suites/mini_chat/):", "```python",
        test_source(tests[0], suite), "```", "", "Failure output:", "```", cluster["detail"] or "(the suite produced no "
        "assertion text for this test: it was skipped, timed out or never ran)", "```", "",
        "Locate the cause in this repository and answer with the JSON object."])


def parse(text: str) -> dict | None:
    return last_json(text, lambda d: d if "locations" in d and d.get("visibility") in VISIBILITY else None, r"\{.*\}")


async def localize(tasks: list[tuple[int, dict]], cache: Path, worktrees: Path, suite: Path) -> None:
    def call(task: tuple[int, dict]):
        pr, cluster = task
        return ask("opus", SYSTEM, prompt(pr, cluster, suite), cwd=worktrees / f"pr-{pr}", agentic=True, timeout_s=1500)

    def record(task: tuple[int, dict], text: str) -> dict | None:
        pr, cluster = task
        result = parse(text)
        return {"pr": pr, "cluster": cluster["key"], "tests": cluster["tests"], **result} if result else None

    await run_cached(tasks, call, record, cache, CONCURRENCY, "localize", every=10)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tests", type=Path, default=OUT / "tests")
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--suite", type=Path, default=Path(os.environ.get("SPEC_BENCH_SUITE", DEFAULT_SUITE)))
    parser.add_argument("--prs", type=int, nargs="*")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    failed = {int(k): v for k, v in json.loads((args.tests / "failed.json").read_text()).items() if v}
    cache = args.tests / "localized.jsonl"
    done = {(r["pr"], r["cluster"]) for r in load_cache(cache)}
    tasks = [(pr, c) for pr in sorted(failed) if not args.prs or pr in args.prs for c in clusters_for(failed[pr])]
    print(f"{len(tasks)} clusters over {len({p for p, _ in tasks})} PRs from {sum(len(failed[p]) for p in failed)} failed tests; "
          f"cached {sum((p, c['key']) in done for p, c in tasks)}")
    for pr in sorted({p for p, _ in tasks}): print(f"  #{pr}: {sum(p == pr for p, _ in tasks)} clusters")
    todo = [(p, c) for p, c in tasks if (p, c["key"]) not in done]
    if args.limit: todo = todo[:args.limit]
    if args.dry_run or not todo: return
    asyncio.run(localize(todo, cache, args.worktrees, args.suite))


if __name__ == "__main__":
    main()
