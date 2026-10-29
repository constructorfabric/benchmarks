"""Cross-review of findings: does another model, checking the code, agree a finding is real and matters?

A stratified sample of each source's findings (Opus, glm run-1, deepseek; single variant) goes to
a validator working in the PR's worktree with Read/Grep/Glob. The validator gets the finding, the
text of the rule it cites (from docs/toolkit-pr-review/rules/) and decides:

  valid-important  the claim is true in this code and the consequence is real enough to fix
                   before merge
  valid-minor      true, but minor, cosmetic, or a matter of taste
  invalid          the claim is wrong, unsupported by the code, or the cited rule does not apply

Validators: Opus through the Claude Code seats, and glm through Claude Code pointed at the LiteLLM
router (the same tools and prompt). Each source is also checked by its own model: that self-review
is the control for a validator rating its own model's findings higher. glm sessions take a slot of
the shared router pool (llm.py, router_slots.py), since the key allows 2 parallel requests.

Verdicts are cached per validator in out/compare/crossval/<validator>.jsonl. The sample is fixed
(seeded) and ordered, so --limit N takes the first N per source for every validator alike.

Usage:
  python -m pr_bench.analysis.cross_validate --findings out/compare/findings --validator opus
  python -m pr_bench.analysis.cross_validate --findings out/compare/findings --validator glm --limit 75
  python -m pr_bench.analysis.cross_validate --findings out/compare/findings --report
"""

from __future__ import annotations

import argparse
import asyncio
import json
import random
import re
from collections import Counter, defaultdict
from pathlib import Path

from pr_bench.analysis.findings import load_findings, render
from pr_bench.analysis.llm import ask, last_json, load_cache, run_cached
from pr_bench.paths import RULES_DIR, WORKTREES

SOURCES = {"opus": ("claude-code/claude-opus-5-5", 1), "glm": ("glm-5.3-flash", 1), "deepseek": ("deepseek-v4.1-flash", 1)}
VERDICTS = ("valid-important", "valid-minor", "invalid")
CONCURRENCY = {"opus": 4, "glm": 2}
BACKEND = {"opus": "opus", "glm": "glm-agent"}

SYSTEM = """You verify one finding from an automated code review of a Rust pull request.
You are inside the repository at the pull request's head. Open the file, read the code around the
lines the finding points at, and follow calls into other files where the claim depends on them.
Judge the claim against the code, not against the reviewer's wording or confidence.
Answer with one of:
- valid-important: the claim is true in this code and the consequence is real enough that it
  should be fixed before merge
- valid-minor: the claim is true, but the effect is minor, cosmetic, or a matter of taste
- invalid: the claim is wrong, not supported by the code, or the cited rule does not apply here
Reply with one JSON object only, no prose around it:
{"verdict": "valid-important|valid-minor|invalid", "reason": "<one sentence>"}"""


def rule_text(check_id: str | None, limit: int = 2500) -> str:
    if not check_id: return "(no rule ID on this finding)"
    family = re.sub(r"-\d+$", "", check_id)
    for path in sorted((RULES_DIR / "rules").glob("*.md")):
        text = path.read_text()
        for pattern in (rf"^### .*\b{re.escape(check_id)}\b.*$", rf"^### .*\b{re.escape(family)}-?\d*.*$"):
            m = re.search(pattern, text, re.M)
            if m:
                end = text.find("\n### ", m.end())
                return text[m.start(): end if end > 0 else None][:limit]
    return f"(rule {check_id} not found in the rule modules)"


def stratified_sample(findings: list[dict], n: int, seed: int = 11) -> list[dict]:
    """Proportional to (severity, rule family) strata, at least one per stratum, fixed order."""
    rng = random.Random(seed)
    strata: dict[tuple, list[dict]] = defaultdict(list)
    for f in findings:
        strata[(f.get("severity") or "-", re.sub(r"-\d+$", "", f.get("check_id") or "UNMAPPED"))].append(f)
    for items in strata.values(): rng.shuffle(items)
    picked, quota = [], {k: max(1, round(n * len(v) / len(findings))) for k, v in strata.items()}
    for k, items in strata.items(): picked += items[:quota[k]]
    rng.shuffle(picked)
    return picked[:n]


def prompt(f: dict) -> str:
    return "\n".join([f"Pull request #{f['pr']}. Finding to verify, in {f['file']}:", render(f), "",
                      "The rule it cites (gears-rust toolkit-pr-review):", rule_text(f.get("check_id")), "",
                      "Verify it in the code and answer with the JSON object."])


def parse(text: str) -> dict | None:
    return last_json(text, lambda d: d if d.get("verdict") in VERDICTS else None)


async def validate(items: list[tuple[str, dict]], validator: str, cache: Path, worktrees: Path) -> None:
    def call(item: tuple[str, dict]):
        _, f = item
        return ask(BACKEND[validator], SYSTEM, prompt(f), cwd=worktrees / f"pr-{f['pr']}", agentic=True, timeout_s=1500)

    def record(item: tuple[str, dict], text: str) -> dict | None:
        src, f = item
        verdict = parse(text)
        return {"finding": f["id"], "source": src, "pr": f["pr"], **verdict} if verdict else None

    await run_cached(items, call, record, cache, CONCURRENCY[validator], validator, every=25)


def build_sample(findings_dir: Path, n: int, path: Path) -> dict[str, list[dict]]:
    """The sample is frozen in `path` on first use, findings included, so every validator and the
    report see the same findings even after the findings are rebuilt (redone jobs get new IDs)."""
    if path.exists(): return json.loads(path.read_text())
    data = load_findings(findings_dir, "single")
    sample = {src: stratified_sample([f for fs in data.get(rev, {}).values() for f in fs], n) for src, rev in SOURCES.items()}
    path.write_text(json.dumps(sample, indent=1))
    return sample


def report(sample: dict[str, list[dict]], out_dir: Path) -> str:
    verdicts = {v: {r["finding"]: r for r in load_cache(out_dir / f"{v}.jsonl")}
                for v in ("opus", "glm") if (out_dir / f"{v}.jsonl").exists()}
    lines = ["# Cross-review of findings", "", "Share of a source's sampled findings a validator rated, after checking the code. "
             "Self-review (a model rating its own findings) is the control for leniency.", "",
             "| source | validator | checked | valid-important | valid-minor | invalid |", "|---|---|---|---|---|---|"]
    for src, items in sample.items():
        for v, got in verdicts.items():
            vs = [got[f["id"]]["verdict"] for f in items if f["id"] in got]
            if not vs: continue
            c = Counter(vs); n = len(vs)
            tag = " (self)" if (src == v) else ""
            lines.append(f"| {src} | {v}{tag} | {n} | {c['valid-important'] / n:.0%} | {c['valid-minor'] / n:.0%} | {c['invalid'] / n:.0%} |")
    if len(verdicts) == 2:
        a, b = verdicts["opus"], verdicts["glm"]
        both = [k for k in a if k in b]
        if both:
            same = sum(a[k]["verdict"] == b[k]["verdict"] for k in both) / len(both)
            imp = [(a[k]["verdict"] == "valid-important", b[k]["verdict"] == "valid-important") for k in both]
            po = sum(x == y for x, y in imp) / len(imp); pa = sum(x for x, _ in imp) / len(imp); pb = sum(y for _, y in imp) / len(imp)
            pe = pa * pb + (1 - pa) * (1 - pb)
            lines += ["", f"Validators agree on {len(both)} findings checked by both: same verdict {same:.0%}; "
                          f"important vs not {po:.0%}, kappa {(po - pe) / (1 - pe) if pe < 1 else 1:.2f}."]
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--findings", type=Path, required=True)
    parser.add_argument("--validator", choices=["opus", "glm"])
    parser.add_argument("--per-source", type=int, default=150)
    parser.add_argument("--limit", type=int, default=0, help="first N of each source's sample")
    parser.add_argument("--worktrees", type=Path, default=WORKTREES)
    parser.add_argument("--report", action="store_true")
    args = parser.parse_args()
    out_dir = args.findings.parent / "crossval"; out_dir.mkdir(parents=True, exist_ok=True)
    sample = build_sample(args.findings, args.per_source, out_dir / "sample.json")
    if args.report:
        text = report(sample, out_dir); (out_dir / "crossval.md").write_text(text); print(text)
        return
    if not args.validator: parser.error("--validator or --report is required")
    cache = out_dir / f"{args.validator}.jsonl"
    done = {r["finding"] for r in load_cache(cache)}
    items = [(s, f) for s, fs in sample.items() for f in (fs[:args.limit] if args.limit else fs) if f["id"] not in done]
    print(f"{args.validator}: {sum(len(fs[:args.limit] if args.limit else fs) for fs in sample.values())} in sample, "
          f"{len(items)} to check; per source: {Counter(s for s, _ in items)}", flush=True)
    if items: asyncio.run(validate(items, args.validator, cache, args.worktrees))


if __name__ == "__main__":
    main()
