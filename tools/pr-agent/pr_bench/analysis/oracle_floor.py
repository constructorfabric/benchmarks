"""Noise floor from the oracle PR: how often does a reviewer flag the reference solution?

PR #72 is the reference implementation of B9-minichat (reward 1.0 on the hidden tests); every
other PR implements the same task. A finding on #72 is not evidence of a defect the tests can
see, so the density of findings a reviewer puts on #72 is a floor for its noise. Compared with
the same reviewer's density on the agent PRs, per severity and per rule class:

  ratio = density on oracle / median density on agent PRs   (findings per KLOC)

A ratio near 1 or above means the reviewer flags the reference solution as often as agent code:
those findings are about conventions, not about what separates a passing solution from a weaker
one. KLOC = thousands of lines the PR adds to Rust sources and Cargo.toml files; the PRs differ
about 10x in size, so counts are compared per KLOC.

Usage: python -m pr_bench.analysis.oracle_floor --findings out/compare/findings [--variant single] [--oracle 72]
Writes oracle_floor.<variant>.md / .json next to the findings.
"""

from __future__ import annotations

import argparse
import json
import re
import statistics
import subprocess
from collections import defaultdict
from functools import cache
from pathlib import Path

from pr_bench.paths import REPO_ROOT

SEVERITIES = ("CRITICAL", "HIGH", "MEDIUM", "LOW")
CLASS = re.compile(r"^([A-Z]+(?:-[A-Z]+)*)-\d+$")


@cache
def rust_kloc(pr: int) -> float:
    """Thousands of lines the PR adds to Rust sources and Cargo.toml files (Cargo.lock is generated, left out)."""
    numstat = subprocess.run(["git", "diff", "--numstat", f"main...pr-{pr}", "--", "*.rs", "*Cargo.toml"],
                             cwd=REPO_ROOT, capture_output=True, text=True, check=True).stdout
    added = sum(int(line.split("\t")[0]) for line in numstat.splitlines() if line.split("\t")[0].isdigit())
    return added / 1000


def reviewer(f: dict) -> str:
    return f"{f['model'].removeprefix('claude-code/')} run-{f['run']}"


def rule_class(f: dict) -> str:
    match = CLASS.match(f.get("check_id") or "")
    return match.group(1) if match else "UNMAPPED"


def density(count: int, pr: int) -> float:
    return count / rust_kloc(pr)


def floor_rows(findings: list[dict], prs: list[int], oracle: int, key) -> list[dict]:
    counts: dict[str, dict[int, int]] = defaultdict(lambda: defaultdict(int))
    for f in findings:
        counts[key(f)][f["pr"]] += 1
    rows = []
    for k, per_pr in sorted(counts.items()):
        agent = [density(per_pr.get(p, 0), p) for p in prs if p != oracle]
        if not agent: continue
        q = statistics.quantiles(agent, n=4) if len(agent) > 1 else [agent[0]] * 3
        med = statistics.median(agent)
        on_oracle = density(per_pr.get(oracle, 0), oracle)
        rows.append({"key": k, "oracle": round(on_oracle, 1), "agent_median": round(med, 1),
                     "agent_q1": round(q[0], 1), "agent_q3": round(q[2], 1),
                     "ratio": round(on_oracle / med, 2) if med else None, "agent_prs": len(agent)})
    return rows


def table(rows: list[dict], first: str) -> list[str]:
    out = [f"| {first} | on oracle | agent PRs median (IQR) | oracle / median |", "|---|---|---|---|"]
    for r in rows:
        ratio = "–" if r["ratio"] is None else f"**{r['ratio']:.2f}**" if r["ratio"] >= 0.8 else f"{r['ratio']:.2f}"
        out.append(f"| {r['key']} | {r['oracle']:.1f} | {r['agent_median']:.1f} ({r['agent_q1']:.1f}–{r['agent_q3']:.1f}) | {ratio} |")
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--findings", type=Path, required=True)
    parser.add_argument("--variant", default="single", choices=["single", "per-module"])
    parser.add_argument("--oracle", type=int, default=72)
    args = parser.parse_args()
    findings = [f for p in sorted(args.findings.glob("findings.*.json")) for f in json.loads(p.read_text())
                if f["variant"] == args.variant]
    by_reviewer: dict[str, list[dict]] = defaultdict(list)
    for f in findings:
        by_reviewer[reviewer(f)].append(f)
    report = [f"# Oracle noise floor ({args.variant}, oracle = PR #{args.oracle})", "",
              "Findings per KLOC (1000 lines added to Rust sources and Cargo.toml). Bold = the reviewer flags "
              "the reference solution at least 80% as often as agent code.", ""]
    out = {}
    for name, fs in sorted(by_reviewer.items()):
        prs = sorted({f["pr"] for f in fs})
        if args.oracle not in prs:
            report += [f"## {name}", "", f"No review of PR #{args.oracle} yet; skipped.", ""]
            continue
        by_sev = floor_rows(fs, prs, args.oracle, lambda f: f["severity"] or "none")
        by_sev.append(floor_rows(fs, prs, args.oracle, lambda f: "all")[0])
        by_class = [r for r in floor_rows(fs, prs, args.oracle, rule_class) if r["agent_median"] > 0 or r["oracle"] > 0]
        out[name] = {"severity": by_sev, "rule_class": by_class, "prs": prs}
        report += [f"## {name} ({len(prs)} PRs)", ""] + table(by_sev, "severity") + [""] + table(by_class, "rule class") + [""]
    (args.findings / f"oracle_floor.{args.variant}.json").write_text(json.dumps(out, indent=1))
    (args.findings / f"oracle_floor.{args.variant}.md").write_text("\n".join(report))
    print("\n".join(report))


if __name__ == "__main__":
    main()
