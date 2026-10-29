"""Does the reviewer see what static analysis sees? code_quality_service (cqscan) issues vs review findings.

The SAST side is the `quality_static` block of the spec-bench scorecards (out/sast/pr-N.json, scope
"changed-lines-vs-base"): SonarQube issues, clippy lints and Semgrep findings on the changed lines.
Only Rust files count, as the reviewers saw only Rust. Production code and tests are reported apart.

1. Span. A lint points at one line. Function-level rules (Sonar S3776 cognitive complexity, clippy
   cognitive_complexity / too_many_lines) point at the signature, so their span is the function body
   (brace matching in the PR head), and a finding anywhere inside it is at distance 0.
2. Candidates. Each SAST issue gets, per reviewer, its findings in the same file at most --max-window
   lines away, nearest first, at most --max-candidates per reviewer.
3. Judge. One Opus call per SAST issue with candidates labels each candidate `same` (the finding
   raises this issue), `related` or `different`. Cache: out/sast/judge.opus.jsonl.
4. Matching per line threshold W, one-to-one per reviewer, nearest first (as in judge_findings.py).

Usage:
  python -m pr_bench.analysis.sast_match --dry-run
  python -m pr_bench.analysis.sast_match              # judge (Claude Code seats)
  python -m pr_bench.analysis.sast_match --report     # out/sast/sast_report.md
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import json
import re
from collections import Counter, defaultdict
from pathlib import Path

from pr_bench.analysis.findings import ALIASES, WINDOWS, code_context, distance, load_findings, span
from pr_bench.analysis.llm import ask, load_cache, parse_labels, run_cached
from pr_bench.paths import OUT, WORKTREES

REVIEWERS = ("opus", "glm1", "glm2", "deepseek")
FUNCTION_RULES = {"rust:S3776", "clippy::cognitive_complexity", "clippy::too_many_lines"}
CONCURRENCY = 4
LABELS = ("same", "related", "different")

SYSTEM = """You compare a static-analysis issue with findings from automated code reviews of one pull request.
For the issue X and each review finding B, decide:
- same: B raises the problem X reports (the same lint, the same complexity concern, the same unsafe call),
  at the same place; fixing B's complaint would also clear X. Wording, severity and rule IDs do not matter.
- related: B is about the same code and touches the concern of X, but makes a different claim.
- different: B is about something else.
Judge from the code shown. Answer with one JSON object only:
{"labels": {"B1": "same|related|different", ...}}"""


def body_end(path: Path, line: int, cap: int = 600) -> int:
    lines = path.read_text(errors="replace").splitlines()
    depth, opened = 0, False
    for n in range(line, min(len(lines), line + cap) + 1):
        text = re.sub(r'"(\\.|[^"\\])*"|//.*$', "", lines[n - 1])
        for ch in text:
            if ch == "{": depth += 1; opened = True
            elif ch == "}": depth -= 1
            if opened and depth == 0: return n
    return line


def tool_items(d: dict) -> list[tuple[str, str, str, str, int]]:
    return ([("sonar", i["rule"], i["message"], i["file"], i["line"]) for i in d["scoped"]["issues"]]
            + [("clippy", i["rule"], i["message"], i["file"], i["line"]) for i in d["rust_scoped"]["lints"]]
            + [("semgrep", i["rule"].split(".")[-1], i["message"], i["file"], i["line"]) for i in d["semgrep_scoped"]["findings"]])


def load_sast(sast_dir: Path, worktrees: Path) -> dict[int, list[dict]]:
    out: dict[int, list[dict]] = defaultdict(list)
    for path in sorted(sast_dir.glob("pr-*.json")):
        pr = int(re.search(r"pr-(\d+)", path.name)[1])
        for tool, rule, msg, file, line in tool_items(json.loads(path.read_text())):
            if not file.endswith(".rs") or not line: continue
            end = body_end(worktrees / f"pr-{pr}" / file, line) if rule in FUNCTION_RULES else line
            sid = hashlib.sha1(f"{pr}|{tool}|{rule}|{file}|{line}|{msg}".encode()).hexdigest()[:16]
            out[pr].append({"id": sid, "pr": pr, "tool": tool, "rule": rule, "message": msg, "file": file,
                            "start_line": line, "end_line": end, "is_test": "/tests/" in file})
    return out


def rule_class(s: dict) -> str:
    if s["rule"] in FUNCTION_RULES: return "complexity (Sonar S3776, clippy cognitive_complexity)"
    if s["tool"] == "semgrep": return f"semgrep {s['rule']}"
    return "other clippy lints"


def build_tasks(sast, data, max_window: int, max_candidates: int) -> list[dict]:
    tasks = []
    for pr, items in sorted(sast.items()):
        for s in items:
            cands = []
            for alias in REVIEWERS:
                pool = data.get(ALIASES[alias], {}).get(pr, [])
                near = sorted(((distance(s, f), abs((f.get("start_line") or 0) - s["start_line"]), f) for f in pool
                               if f["file"] == s["file"] and distance(s, f) <= max_window), key=lambda x: x[:2])
                cands += [{"reviewer": alias, "d": d, "b": f} for d, _, f in near[:max_candidates]]
            if cands: tasks.append({"pr": pr, "s": s, "cands": cands})
    return tasks


def task_key(task: dict) -> str:
    return hashlib.sha1((task["s"]["id"] + ":" + ",".join(c["b"]["id"] for c in task["cands"])).encode()).hexdigest()[:20]


def prompt(task: dict, worktrees: Path) -> str:
    s, cands = task["s"], task["cands"]
    lo = min([s["start_line"]] + [span(c["b"])[0] for c in cands])
    hi = max([s["end_line"]] + [span(c["b"])[1] for c in cands])
    parts = [f"Pull request #{task['pr']}, file {s['file']}", "", "Code at the PR head:", "```rust",
             code_context(task["pr"], s["file"], lo, hi, worktrees, cap=200), "```", "",
             f"Static-analysis issue X ({s['tool']} {s['rule']}), line {s['start_line']}:", s["message"], ""]
    for i, c in enumerate(cands, 1):
        b = c["b"]; b0, b1 = span(b)
        parts.append(f"Review finding B{i}: lines {b0}-{b1} | {b.get('check_id') or '-'} {b.get('severity') or ''} | "
                     f"{b.get('title') or ''}\n{(b.get('body') or '')[:1000]}\n")
    parts.append('Return {"labels": {' + ", ".join(f'"B{i}": "..."' for i in range(1, len(cands) + 1)) + "}}")
    return "\n".join(parts)


async def judge_all(tasks: list[dict], cache: Path, worktrees: Path) -> None:
    done = {r["key"] for r in load_cache(cache)}
    todo = [t for t in tasks if task_key(t) not in done]
    print(f"opus: {len(tasks)} tasks, {len(tasks) - len(todo)} cached, {len(todo)} to call", flush=True)

    def record(t: dict, text: str) -> dict | None:
        labels = parse_labels(text, len(t["cands"]), LABELS)
        if not labels: return None
        return {"key": task_key(t), "pr": t["pr"], "s": t["s"]["id"],
                "edges": [{"reviewer": c["reviewer"], "b": c["b"]["id"], "d": c["d"], "label": labels[f"B{i}"]}
                          for i, c in enumerate(t["cands"], 1)]}

    await run_cached(todo, lambda t: ask("opus", SYSTEM, prompt(t, worktrees)), record, cache, CONCURRENCY, "sast judge")


def matches(tasks: list[dict], verdicts: dict, reviewer: str, window: int, accept: tuple[str, ...]) -> tuple[set, set]:
    edges = []
    for t in tasks:
        v = verdicts.get(task_key(t))
        if not v: continue
        edges += [(e["d"], t["s"]["id"], e["b"]) for e in v["edges"]
                  if e["reviewer"] == reviewer and e["label"] in accept and e["d"] <= window]
    used_s, used_b = set(), set()
    for _, s, b in sorted(edges):
        if s in used_s or b in used_b: continue
        used_s.add(s); used_b.add(b)
    return used_s, used_b


def pct(n: int, d: int) -> str:
    return f"{n} ({100 * n / d:.0f}%)" if d else "0"


def report(sast, data, tasks: list[dict], verdicts: dict) -> str:
    items = [s for v in sast.values() for s in v]
    judged = sum(task_key(t) in verdicts for t in tasks)
    lines = ["# Static analysis (code_quality_service) vs review findings, single variant", "",
             f"SAST issues on changed Rust lines: {len(items)} in {len(sast)} PRs "
             f"({sum(not s['is_test'] for s in items)} in production code, {sum(s['is_test'] for s in items)} in tests). "
             f"Issues with a reviewer finding within {max(WINDOWS)} lines: {len(tasks)}; judged: {judged}.",
             "SonarQube found no VULNERABILITY, SECURITY_HOTSPOT or BUG issue and cargo-audit no introduced advisory "
             "in any PR; Semgrep's security findings on Rust are temp-dir use in test helpers.", ""]
    classes = sorted({rule_class(s) for s in items})
    for accept, name in ((("same",), "same"), (("same", "related"), "same or related")):
        lines += [f"## SAST issues the reviewer also raised ({name}), W = {', '.join(map(str, WINDOWS))} lines", "",
                  "| class | scope | SAST issues | " + " | ".join(REVIEWERS) + " |", "|---|---|---|" + "---|" * len(REVIEWERS)]
        for cls in classes:
            for is_test in (False, True):
                ids = {s["id"] for s in items if rule_class(s) == cls and s["is_test"] == is_test}
                if not ids: continue
                cells = []
                for r in REVIEWERS:
                    per_w = [len(matches(tasks, verdicts, r, w, accept)[0] & ids) for w in WINDOWS]
                    cells.append(" / ".join(map(str, per_w)))
                lines.append(f"| {cls} | {'tests' if is_test else 'production'} | {len(ids)} | " + " | ".join(cells) + " |")
        lines.append("")
    lines += ["## Review findings that coincide with a SAST issue (same, W = 3)", "",
              "| reviewer | findings in the PRs (Rust) | coincide with SAST | of them complexity |", "|---|---|---|---|"]
    by_id = {s["id"]: s for s in items}
    for r in REVIEWERS:
        total = sum(len(v) for pr, v in data.get(ALIASES[r], {}).items() if pr in sast)
        used_s, used_b = matches(tasks, verdicts, r, 3, ("same",))
        cx = sum(rule_class(by_id[s]).startswith("complexity") for s in used_s)
        lines.append(f"| {r} | {total} | {pct(len(used_b), total)} | {cx} |")
    top = Counter(s["rule"] for s in items).most_common(12)
    lines += ["", "## SAST rules (Rust, changed lines)", "", "| rule | issues | caught by any reviewer (same, W = 3) |", "|---|---|---|"]
    caught = set().union(*(matches(tasks, verdicts, r, 3, ("same",))[0] for r in REVIEWERS))
    for rule, n in top:
        lines.append(f"| {rule} | {n} | {sum(s['id'] in caught for s in items if s['rule'] == rule)} |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--sast", type=Path, default=OUT / "sast")
    parser.add_argument("--findings", type=Path, default=OUT / "compare" / "findings")
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--max-window", type=int, default=max(WINDOWS))
    parser.add_argument("--max-candidates", type=int, default=4, help="per reviewer")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--report", action="store_true")
    args = parser.parse_args()
    sast = load_sast(args.sast, args.worktrees)
    data = load_findings(args.findings, "single")
    tasks = build_tasks(sast, data, args.max_window, args.max_candidates)
    cache = args.sast / "judge.opus.jsonl"
    if args.report:
        verdicts = {r["key"]: r for r in load_cache(cache)}
        text = report(sast, data, tasks, verdicts)
        (args.sast / "sast_report.md").write_text(text)
        print(text)
        return
    n_items = sum(map(len, sast.values()))
    print(f"{n_items} SAST issues, {len(tasks)} judge calls, {sum(len(t['cands']) for t in tasks)} candidates, "
          f"~{sum(len(prompt(t, args.worktrees)) for t in tasks) / 4 / 1e6:.1f}M prompt tokens", flush=True)
    if args.dry_run: return
    asyncio.run(judge_all(tasks, cache, args.worktrees))


if __name__ == "__main__":
    main()
