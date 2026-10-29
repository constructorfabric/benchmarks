"""Do two reviewers report the same problem? Line-window candidates, confirmed by an LLM judge.

Location alone counts two findings as one when they sit within a few lines,
whatever they say. This adds the meaning:

1. Candidates. For a pair of reviewers on one PR, every finding of A gets the findings of B in the
   same file whose line range is at most --max-window lines away (0 = overlapping), nearest first,
   at most --max-candidates of them.
2. Judge. One call per finding of A that has candidates: A, the candidates and the PR's code around
   them go to the judge, which labels each candidate `same` (the same defect: fixing one fixes the
   other), `related` (same code, overlapping concern, a different claim) or `different`.
3. Matching. Per line threshold W, `same` edges within W lines are matched one-to-one, nearest
   first. A's side is the reviewer with fewer findings (fewer calls); the label is symmetric, so
   both directions come out of the same calls.

Two judges: Opus through the Claude Code seats (`--judge opus`, all pairs) and glm through the router
(`--judge glm --sample 300`, a random subset of A findings), so the report can state how far the
judges agree. Verdicts are cached per judge in out/compare/judge/<judge>.jsonl; a rerun only calls
what is missing.

Usage:
  python -m pr_bench.analysis.judge_findings --findings out/compare/findings --judge opus --dry-run
  python -m pr_bench.analysis.judge_findings --findings out/compare/findings --judge opus
  python -m pr_bench.analysis.judge_findings --findings out/compare/findings --judge glm --sample 300
  python -m pr_bench.analysis.judge_findings --findings out/compare/findings --report
"""

from __future__ import annotations

import argparse
import asyncio
import hashlib
import random
import re
from collections import defaultdict
from pathlib import Path

from pr_bench.analysis.findings import ALIASES, WINDOWS, code_context, distance, load_findings, render, span
from pr_bench.analysis.llm import ask, load_cache, parse_labels, run_cached
from pr_bench.paths import WORKTREES

LABELS = ("same", "related", "different")
DEFAULT_PAIRS = ("opus:glm1", "opus:deepseek", "glm1:glm2", "glm1:deepseek")
JUDGES = ("opus", "glm")
CONCURRENCY = {"opus": 4, "glm": 2}

SYSTEM = """You compare findings from automated code reviews of one pull request.
For the finding A and each candidate B, decide whether they report the same problem:
- same: the same underlying defect or rule violation at the same place; fixing one would fix the other.
  Different wording, severity or rule ID does not matter.
- related: the same code and an overlapping concern, but a different claim or a different defect.
- different: unrelated problems that only happen to be near each other.
Judge from the code shown, not from the reviewers' confidence. Answer with one JSON object only:
{"labels": {"B1": "same|related|different", ...}}"""


def build_tasks(data, pairs, max_window: int, max_candidates: int) -> list[dict]:
    tasks = []
    for pair in pairs:
        ra, rb = (ALIASES[x] for x in pair.split(":"))
        if ra not in data or rb not in data: continue
        if sum(map(len, data[ra].values())) > sum(map(len, data[rb].values())): ra, rb = rb, ra
        for pr in sorted(set(data[ra]) & set(data[rb])):
            for a in data[ra][pr]:
                cands = sorted(((distance(a, b), b) for b in data[rb][pr]
                                if b["file"] == a["file"] and distance(a, b) <= max_window), key=lambda x: x[0])
                if not cands: continue
                cands = cands[:max_candidates]
                tasks.append({"pair": pair, "a_side": ra, "b_side": rb, "pr": pr, "a": a,
                              "cands": [{"d": d, "b": b} for d, b in cands]})
    return tasks


def task_key(task: dict) -> str:
    return hashlib.sha1((task["a"]["id"] + ":" + ",".join(c["b"]["id"] for c in task["cands"])).encode()).hexdigest()[:20]


def prompt(task: dict, worktrees: Path) -> str:
    a, cands = task["a"], task["cands"]
    lo = min([span(a)[0]] + [span(c["b"])[0] for c in cands]); hi = max([span(a)[1]] + [span(c["b"])[1] for c in cands])
    parts = [f"Pull request #{task['pr']}, file {a['file']}", "", "Code at the PR head:", "```rust",
             code_context(task["pr"], a["file"], lo, hi, worktrees), "```", "", "Finding A:", render(a), ""]
    parts += [f"Candidate B{i}:\n{render(c['b'])}\n" for i, c in enumerate(cands, 1)]
    parts.append('Return {"labels": {' + ", ".join(f'"B{i}": "..."' for i in range(1, len(cands) + 1)) + "}}")
    return "\n".join(parts)


async def judge_all(tasks: list[dict], judge: str, cache: Path, worktrees: Path) -> None:
    done = {r["key"] for r in load_cache(cache)}
    todo = [t for t in tasks if task_key(t) not in done]
    print(f"{judge}: {len(tasks)} tasks, {len(tasks) - len(todo)} cached, {len(todo)} to call", flush=True)

    def record(t: dict, text: str) -> dict | None:
        labels = parse_labels(text, len(t["cands"]), LABELS)
        if not labels: return None
        return {"key": task_key(t), "pair": t["pair"], "pr": t["pr"], "a": t["a"]["id"],
                "edges": [{"b": c["b"]["id"], "d": c["d"], "label": labels[f"B{i}"]} for i, c in enumerate(t["cands"], 1)]}

    await run_cached(todo, lambda t: ask(judge, SYSTEM, prompt(t, worktrees)), record, cache, CONCURRENCY[judge], judge)


def load_verdicts(cache: Path) -> dict[str, dict]:
    return {r["key"]: r for r in load_cache(cache)}


def match_counts(tasks: list[dict], verdicts: dict[str, dict], window: int, accept: tuple[str, ...]) -> tuple[int, set, set]:
    edges = []
    for t in tasks:
        v = verdicts.get(task_key(t))
        if not v: continue
        edges += [(e["d"], t["a"]["id"], e["b"]) for e in v["edges"] if e["label"] in accept and e["d"] <= window]
    used_a, used_b = set(), set()
    for _, a, b in sorted(edges):
        if a in used_a or b in used_b: continue
        used_a.add(a); used_b.add(b)
    return len(used_a), used_a, used_b


def location_counts(tasks: list[dict], window: int) -> int:
    used_a, used_b = set(), set()
    for d, a, b in sorted((c["d"], t["a"]["id"], c["b"]["id"]) for t in tasks for c in t["cands"]):
        if d > window or a in used_a or b in used_b: continue
        used_a.add(a); used_b.add(b)
    return len(used_a)


def report(data, tasks: list[dict], findings_dir: Path, judges: list[str], out_dir: Path) -> str:
    verdicts = {j: load_verdicts(out_dir / f"{j}.jsonl") for j in judges}
    lines = ["# Same problem or not: line windows + LLM judge", "",
             "Share of each side's findings that the other reviewer also reported, on the PRs both reviewed.",
             "`location` = nearest finding within W lines in the same file, no judge; `judge: same` = the judge "
             "confirmed the same defect; `+related` also accepts overlapping concerns.", ""]
    by_pair = defaultdict(list)
    for t in tasks: by_pair[t["pair"]].append(t)
    for pts in by_pair.values():
        ra, rb = pts[0]["a_side"], pts[0]["b_side"]
        common = sorted(set(data[ra]) & set(data[rb]))
        n_a = sum(len(data[ra][p]) for p in common); n_b = sum(len(data[rb][p]) for p in common)
        name = lambda r: f"{r[0].removeprefix('claude-code/')} run-{r[1]}"
        lines += [f"## {name(ra)} ↔ {name(rb)}", "",
                  f"{len(common)} common PRs; {n_a} findings of {name(ra)}, {n_b} of {name(rb)}; "
                  f"{len(pts)} findings of {name(ra)} had candidates within the window.", "",
                  f"| W lines | measure | of {name(ra)} | of {name(rb)} |", "|---|---|---|---|"]
        for w in WINDOWS:
            m = location_counts(pts, w)
            lines.append(f"| {w} | location | {m / n_a:.0%} | {m / n_b:.0%} |")
            for j in judges:
                judged = [t for t in pts if task_key(t) in verdicts[j]]
                if not judged: continue
                # a sampled judge saw a fraction of the A findings with candidates: scale its matches up
                fraction = len(judged) / len(pts)
                cover = f" (sample {len(judged)}/{len(pts)})" if fraction < 1 else ""
                for accept, label in ((("same",), "same"), (("same", "related"), "+related")):
                    m, _, _ = match_counts(judged, verdicts[j], w, accept)
                    est = m / fraction
                    lines.append(f"| {w} | judge {j}: {label}{cover} | {est / n_a:.0%} | {est / n_b:.0%} |")
        lines.append("")
    lines += averaged(data, by_pair, verdicts.get("opus") or next(iter(verdicts.values()), {}))
    if len(judges) == 2 and all(verdicts.values()):
        a, b = (verdicts[j] for j in judges)
        pairs_ = [(ea["label"], eb["label"]) for k in set(a) & set(b) for ea, eb in zip(a[k]["edges"], b[k]["edges"], strict=True)]
        if pairs_:
            agree = sum(x == y for x, y in pairs_) / len(pairs_)
            po = sum((x == "same") == (y == "same") for x, y in pairs_) / len(pairs_)
            pa = sum(x == "same" for x, _ in pairs_) / len(pairs_); pb = sum(y == "same" for _, y in pairs_) / len(pairs_)
            pe = pa * pb + (1 - pa) * (1 - pb)
            kappa = (po - pe) / (1 - pe) if pe < 1 else 1.0
            lines += ["## Judge agreement", "",
                      f"{len(pairs_)} candidate pairs judged by both: identical label {agree:.0%}; "
                      f"same vs not-same: {po:.0%} agreement, Cohen's kappa {kappa:.2f} "
                      f"({judges[0]} says same {pa:.0%}, {judges[1]} {pb:.0%}).", ""]
    return "\n".join(lines)


def family(alias: str) -> str:
    return re.sub(r"\d+$", "", alias)


def averaged(data, by_pair: dict[str, list[dict]], verdicts: dict[str, dict], window: int = 3) -> list[str]:
    """Pairs of the same kind averaged over runs: glm↔opus over glm's runs, glm↔glm over all run pairs.

    Shares are oriented by family (e.g. always "glm's findings also in Opus"), whichever side each
    pair's A was. Each run pair is compared on the PRs both runs reviewed and only counts once its
    candidates are fully judged."""
    groups: dict[tuple[str, str], list[tuple[float, float, int]]] = defaultdict(list)
    for pair, pts in by_pair.items():
        if any(task_key(t) not in verdicts for t in pts): continue
        x, y = pair.split(":")
        fx, fy = sorted((family(x), family(y)))
        if (family(x), family(y)) != (fx, fy): x, y = y, x
        rx, ry = ALIASES[x], ALIASES[y]
        common = set(data[rx]) & set(data[ry])
        n_x = sum(len(data[rx][p]) for p in common); n_y = sum(len(data[ry][p]) for p in common)
        m, _, _ = match_counts(pts, verdicts, window, ("same",))
        groups[(fx, fy)].append((m / n_x, m / n_y, len(common)))
    if not groups: return []
    name = {"opus": "Opus", "glm": "glm", "deepseek": "deepseek"}
    out = ["## Averaged over runs", "",
           f"Judge `same`, W = {window}. Mean over the run pairs of each kind, range in brackets; "
           "a pair counts once all its candidates are judged.", "",
           "| X ↔ Y | run pairs | X's findings also in Y | Y's findings also in X |", "|---|---|---|---|"]
    for (fx, fy), vals in sorted(groups.items()):
        a = [v[0] for v in vals]; b = [v[1] for v in vals]
        fmt = lambda xs: f"{sum(xs) / len(xs):.0%} ({min(xs):.0%}–{max(xs):.0%})" if len(xs) > 1 else f"{xs[0]:.0%}"
        out.append(f"| {name.get(fx, fx)} ↔ {name.get(fy, fy)} | {len(vals)} | {fmt(a)} | {fmt(b)} |")
    return out + [""]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--findings", type=Path, required=True)
    parser.add_argument("--variant", default="single", choices=["single", "per-module"])
    parser.add_argument("--pairs", nargs="+", default=list(DEFAULT_PAIRS), help=f"aliases: {', '.join(ALIASES)}")
    parser.add_argument("--judge", choices=JUDGES)
    parser.add_argument("--sample", type=int, default=0, help="judge only N random A findings (with candidates)")
    parser.add_argument("--max-window", type=int, default=max(WINDOWS))
    parser.add_argument("--max-candidates", type=int, default=8)
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--report", action="store_true", help="write judge_report.md from the cached verdicts")
    args = parser.parse_args()
    out_dir = args.findings.parent / "judge"; out_dir.mkdir(parents=True, exist_ok=True)
    data = load_findings(args.findings, args.variant)
    tasks = build_tasks(data, args.pairs, args.max_window, args.max_candidates)
    if args.report:
        judges = [j for j in JUDGES if (out_dir / f"{j}.jsonl").exists()]
        text = report(data, tasks, args.findings, judges, out_dir)
        (out_dir / f"judge_report.{args.variant}.md").write_text(text)
        print(text)
        return
    if not args.judge: parser.error("--judge or --report is required")
    if args.sample: tasks = random.Random(13).sample(tasks, min(args.sample, len(tasks)))
    per_pair = defaultdict(int)
    for t in tasks: per_pair[t["pair"]] += 1
    cands = sum(len(t["cands"]) for t in tasks)
    print(f"{len(tasks)} judge calls ({dict(per_pair)}), {cands} candidate pairs, "
          f"~{sum(len(prompt(t, args.worktrees)) for t in tasks) / 4 / 1e6:.1f}M prompt tokens")
    if args.dry_run: return
    asyncio.run(judge_all(tasks, args.judge, out_dir / f"{args.judge}.jsonl", args.worktrees))


if __name__ == "__main__":
    main()
