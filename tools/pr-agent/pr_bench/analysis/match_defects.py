"""Recall against test-confirmed defects: did a reviewer flag the cause of a failed hidden test?

Input: the root causes from localize_failures.py (out/tests/localized.jsonl; one defect per cluster
of failed tests in one PR) and the reviewers' findings (to_findings.py). For every defect and every
reviewer run, the candidates are that run's findings on the same PR in a root-cause or related
file, within --window lines of one of those ranges (at most 10, nearest first). With no candidate
the defect counts as missed without a call. Otherwise Opus (Claude Code seats) judges each candidate:

  catches   the finding names this defect or its direct cause; acting on it would fix the test
  partial   the same code and a related concern; it would lead a developer to the defect
  no        unrelated

Verdicts are cached in out/tests/defect_judge.<variant>.jsonl. --report prints recall per reviewer (mean and
range over runs) by visibility class (code_evident / spec_dependent / missing_feature).

Usage:
  python -m pr_bench.analysis.match_defects --findings out/compare/findings --dry-run
  python -m pr_bench.analysis.match_defects --findings out/compare/findings
  python -m pr_bench.analysis.match_defects --findings out/compare/findings --report
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
from collections import defaultdict
from pathlib import Path

from pr_bench.analysis.findings import code_context, load_findings, render, span
from pr_bench.analysis.llm import ask, load_cache, parse_labels, run_cached
from pr_bench.paths import OUT, WORKTREES

CONCURRENCY = 4
LABELS = ("catches", "partial", "no")
VISIBILITY = ("code_evident", "spec_dependent", "missing_feature")

SYSTEM = """You check whether an automated code review flagged a known defect.
The defect was found by a failing end-to-end test and its root cause was located in the code.
For each review finding (candidate), decide:
- catches: the finding names this defect or its direct cause; a developer acting on the finding
  would fix the failing behaviour.
- partial: the finding is about the same code and a related concern, and would lead a developer
  to the defect, but does not name it.
- no: the finding is about something else.
Answer with one JSON object only: {"labels": {"F1": "catches|partial|no", ...}}"""


def load_defects(tests_dir: Path) -> list[dict]:
    defects = {}
    for line in (tests_dir / "localized.jsonl").open():
        r = json.loads(line)
        defects[(r["pr"], r["cluster"])] = r
    failed = json.loads((tests_dir / "failed.json").read_text())
    for d in defects.values():
        details = {f["test"]: f.get("detail", "") for f in failed.get(str(d["pr"]), [])}
        d["detail"] = next((details[t] for t in d["tests"] if details.get(t)), "")
        d["id"] = hashlib.sha1(f"{d['pr']}|{d['cluster']}".encode()).hexdigest()[:16]
    return list(defects.values())


def gap(lo: int, hi: int, f: dict) -> int:
    s, e = span(f)
    return max(0, max(lo, s) - min(hi, e))


def candidates(defect: dict, findings: list[dict], window: int, cap: int = 10) -> list[tuple[int, dict]]:
    out = []
    for f in findings:
        ds = [gap(loc["start_line"] or 0, loc["end_line"] or loc["start_line"] or 0, f)
              for loc in defect["locations"] if loc.get("file") == f["file"]]
        if ds and min(ds) <= window: out.append((min(ds), f))
    return sorted(out, key=lambda x: x[0])[:cap]


def reviewers(data: dict) -> list[tuple[str, int]]:
    return sorted(data)


def build_tasks(defects, data, window: int) -> tuple[list[dict], dict]:
    tasks, misses = [], defaultdict(set)
    for d in defects:
        for rev in reviewers(data):
            if d["pr"] not in data[rev]: continue
            cands = candidates(d, data[rev][d["pr"]], window)
            if cands: tasks.append({"defect": d, "reviewer": rev, "cands": cands})
            else: misses[rev].add(d["id"])
    return tasks, misses


def task_key(t: dict) -> str:
    raw = t["defect"]["id"] + ":" + ",".join(f["id"] for _, f in t["cands"])
    return hashlib.sha1(raw.encode()).hexdigest()[:20]


def prompt(t: dict, worktrees: Path) -> str:
    d = t["defect"]
    roots = [loc for loc in d["locations"] if loc.get("role") == "root_cause"] or d["locations"]
    parts = [f"Pull request #{d['pr']}. Defect found by {len(d['tests'])} failing test(s), e.g. {d['tests'][0]}",
             f"Root cause (located in the code): {d['cause']}", "", "Test failure output:", "```",
             (d["detail"] or "(no assertion text)")[:1500], "```", ""]
    for loc in roots[:2]:
        parts += [f"Code at the root cause, {loc['file']}:", "```rust",
                  code_context(d["pr"], loc["file"], loc["start_line"] or 1, loc["end_line"] or loc["start_line"] or 1, worktrees, cap=60), "```", ""]
    parts += [f"Finding F{i} ({f['file']}):\n{render(f)}\n" for i, (_, f) in enumerate(t["cands"], 1)]
    parts.append('Return {"labels": {' + ", ".join(f'"F{i}": "..."' for i in range(1, len(t["cands"]) + 1)) + "}}")
    return "\n".join(parts)


async def judge(tasks: list[dict], cache: Path, worktrees: Path) -> None:
    done = {r["key"] for r in load_cache(cache)}
    todo = [t for t in tasks if task_key(t) not in done]
    print(f"{len(tasks)} tasks, {len(tasks) - len(todo)} cached, {len(todo)} to call", flush=True)

    def record(t: dict, text: str) -> dict | None:
        labels = parse_labels(text, len(t["cands"]), LABELS, prefix="F")
        if not labels: return None
        return {"key": task_key(t), "defect": t["defect"]["id"], "reviewer": list(t["reviewer"]),
                "labels": [{"finding": f["id"], "d": dist, "label": labels[f"F{i}"]} for i, (dist, f) in enumerate(t["cands"], 1)]}

    await run_cached(todo, lambda t: ask("opus", SYSTEM, prompt(t, worktrees)), record, cache, CONCURRENCY, "recall judge")


def report(defects, data, tasks, cache: Path) -> str:
    verdicts = {r["key"]: r for r in load_cache(cache)}
    best: dict[tuple, str] = {}
    for t in tasks:
        v = verdicts.get(task_key(t))
        if not v: continue
        labels = {x["label"] for x in v["labels"]}
        best[(t["reviewer"], t["defect"]["id"])] = "catches" if "catches" in labels else "partial" if "partial" in labels else "no"
    pending = sum(task_key(t) not in verdicts for t in tasks)
    by_model: dict[str, list[tuple[int, dict]]] = defaultdict(list)
    for rev in reviewers(data):
        model, run = rev
        covered = [d for d in defects if d["pr"] in data[rev]]
        row = {}
        for vis in ("all",) + VISIBILITY:
            ds = [d for d in covered if vis == "all" or d["visibility"] == vis]
            c = sum(best.get((rev, d["id"])) == "catches" for d in ds)
            p = sum(best.get((rev, d["id"])) in ("catches", "partial") for d in ds)
            row[vis] = (c, p, len(ds))
        by_model[model.removeprefix("claude-code/")].append((run, row))
    lines = ["# Recall against test-confirmed defects", "",
             f"{len(defects)} defects (clusters of failed hidden tests, one PR each). Share a reviewer flagged: "
             "`catches` / `catches+partial`. Several runs: mean, range in brackets." + (f" {pending} judge tasks pending." if pending else ""), "",
             "| reviewer | runs | all | code_evident | spec_dependent | missing_feature |", "|---|---|---|---|---|---|"]
    for model, rows in sorted(by_model.items()):
        cells = []
        for vis in ("all",) + VISIBILITY:
            cs = [r[vis][0] / r[vis][2] for _, r in rows if r[vis][2]]
            ps = [r[vis][1] / r[vis][2] for _, r in rows if r[vis][2]]
            n = rows[0][1][vis][2]
            if not cs: cells.append("–"); continue
            rng = f" ({min(cs):.0%}–{max(cs):.0%})" if len(cs) > 1 else ""
            cells.append(f"{sum(cs) / len(cs):.0%}{rng} / {sum(ps) / len(ps):.0%} · n={n}")
        lines.append(f"| {model} | {len(rows)} | " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--findings", type=Path, required=True)
    parser.add_argument("--tests", type=Path, default=OUT / "tests")
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--variant", default="single", choices=["single", "per-module"])
    parser.add_argument("--window", type=int, default=25)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--report", action="store_true")
    args = parser.parse_args()
    defects = load_defects(args.tests)
    data = load_findings(args.findings, args.variant)
    tasks, misses = build_tasks(defects, data, args.window)
    cache = args.tests / f"defect_judge.{args.variant}.jsonl"
    if args.report:
        text = report(defects, data, tasks, cache)
        (args.tests / f"recall.{args.variant}.md").write_text(text)
        print(text)
        return
    per_rev = defaultdict(int)
    for t in tasks: per_rev[f"{t['reviewer'][0].removeprefix('claude-code/')} run-{t['reviewer'][1]}"] += 1
    print(f"{len(defects)} defects; {len(tasks)} judge calls; per reviewer: {dict(per_rev)}")
    print("no candidate at all (missed without a call): " + ", ".join(
        f"{m.removeprefix('claude-code/')} run-{r}: {len(v)}" for (m, r), v in sorted(misses.items())))
    if args.dry_run: return
    asyncio.run(judge(tasks, cache, args.worktrees))


if __name__ == "__main__":
    main()
