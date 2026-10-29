"""Collect run_review results into one findings.json per model plus a run summary.

findings.json follows the CLLM-559 layout: a flat list of findings, each tagged with
`reviewer = pr-agent/<model>` and the run it came from, so two runs of the same model
and two models on the same tool can be compared afterwards.

Usage: python -m pr_bench.analysis.to_findings --matrix out/full out/pilot3 out/opus --out out/compare/findings
"""

from __future__ import annotations

import argparse
import json
import re
from collections import defaultdict
from pathlib import Path

CHECK_ID = re.compile(r"\s*([A-Z]+(?:-[A-Z]+)*-\d+)\s*(?:[:\-—]\s*)?(.*)", re.S)
SEVERITY = re.compile(r"\b(CRITICAL|HIGH|MEDIUM|LOW|INFO)\b")


def split_header(header: str) -> tuple[str | None, str]:
    match = CHECK_ID.match(header or "")
    if not match: return None, (header or "").strip()
    return match.group(1), match.group(2).strip()


BODY_PREFIX = re.compile(r"^\[([A-Z]+(?:-[A-Z]+)*-\d+)\]\[([A-Z]+)\]\s*", re.S)


def finding_from_issue(issue: dict, result: dict) -> dict:
    check_id, title = split_header(str(issue.get("issue_header", "")))
    content = str(issue.get("issue_content", "")).strip()
    prefix = BODY_PREFIX.match(content)
    if prefix:
        check_id = check_id or prefix.group(1)
        content = content[prefix.end():]
    severity = prefix if prefix else SEVERITY.search(content)
    severity_label = prefix.group(2) if prefix else (severity.group(1) if severity else None)
    return {
        "reviewer": f"pr-agent/{result['model']}",
        "model": result["model"],
        "variant": result["variant"],
        "module": result["module"],
        "run": result["run"],
        "pr": result["pr"],
        "head_branch": result.get("head_branch"),
        "file": str(issue.get("relevant_file") or "").strip() or None,
        "start_line": issue.get("start_line"),
        "end_line": issue.get("end_line"),
        "check_id": check_id,
        "severity": severity_label,
        "title": title,
        "body": content,
    }


def job_key(result: dict) -> tuple:
    return result["pr"], result["model"], result["run"], result["variant"], result["module"]


def load_results(matrix_dirs: list[Path]) -> list[dict]:
    """Results from several matrix dirs; the first dir that holds a job wins, so a pilot and a
    later full run of the same (pr, model, run, variant, module) are never counted twice."""
    seen: dict[tuple, dict] = {}
    for matrix_dir in matrix_dirs:
        for p in sorted(matrix_dir.rglob("result.json")):
            result = json.loads(p.read_text())
            seen.setdefault(job_key(result), result)
    return list(seen.values())


def summarize(result: dict) -> dict:
    calls = result["calls"]
    return {
        "pr": result["pr"], "model": result["model"], "variant": result["variant"], "module": result["module"],
        "run": result["run"], "seconds_total": result["seconds_total"], "calls": len(calls),
        "chunks": result["chunks"], "failed_chunks": result["failed_chunks"],
        "leaked_calls": sum(c["reasoning_leaked"] for c in calls),
        "unparsed_raw_calls": sum(not c["raw_parses"] for c in calls),
        "unparsed_after_fix": sum(not c["fixed_parses"] for c in calls),
        "prompt_chars": sum(c["prompt_chars"] for c in calls),
        "response_chars": sum(c["response_chars"] for c in calls),
        "files_reviewed": result["files_reviewed"],
        "files_skipped_not_rust": len(result["files_skipped_not_rust"]),
        "files_omitted_by_budget": len(result["files_omitted_by_budget"]),
        "findings": result["findings_count"],
        "check_id_counts": result["check_id_counts"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--matrix", type=Path, nargs="+", required=True, help="one or more matrix dirs; earlier wins")
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    results = load_results(args.matrix)
    if not results: parser.error(f"no result.json under {args.matrix}")
    by_model: dict[str, list[dict]] = defaultdict(list)
    for result in results:
        issues = result.get("review", {}).get("key_issues_to_review") or []
        by_model[result["model"]].extend(finding_from_issue(i, result) for i in issues if isinstance(i, dict))
    for model, findings in by_model.items():
        path = args.out / f"findings.{model.replace('/', '_')}.json"
        path.write_text(json.dumps(findings, indent=1, ensure_ascii=False))
        print(f"{path}: {len(findings)} findings")
    summary = [summarize(r) for r in results]
    (args.out / "summary.json").write_text(json.dumps(summary, indent=1))
    print(f"{args.out / 'summary.json'}: {len(summary)} runs")


if __name__ == "__main__":
    main()
