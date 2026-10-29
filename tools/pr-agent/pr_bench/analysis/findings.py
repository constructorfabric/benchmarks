"""Findings as the analysis scripts read them: loading, IDs, line spans, code around a finding."""

from __future__ import annotations

import hashlib
import json
from collections import defaultdict
from pathlib import Path

WINDOWS = (0, 3, 10, 25)
# reviewer alias -> (model, run); the aliases are what --pairs and the reports use
ALIASES = {
    "opus": ("claude-code/claude-opus-5-5", 1), "deepseek": ("deepseek-v4.1-flash", 1),
    **{f"glm{k}": ("glm-5.3-flash", k) for k in range(1, 6)},
}


def finding_id(f: dict) -> str:
    raw = "|".join(str(f.get(k)) for k in ("model", "variant", "run", "pr", "file", "start_line", "end_line", "title"))
    return hashlib.sha1((raw + (f.get("body") or "")[:200]).encode()).hexdigest()[:16]


def span(f: dict) -> tuple[int, int]:
    s = f.get("start_line") or 0
    return s, f.get("end_line") or s


def distance(a: dict, b: dict) -> int:
    (a0, a1), (b0, b1) = span(a), span(b)
    return max(0, max(a0, b0) - min(a1, b1))


def load_findings(findings_dir: Path, variant: str) -> dict[tuple[str, int], dict[int, list[dict]]]:
    """(model, run) -> pr -> findings, for one prompt variant."""
    out: dict[tuple[str, int], dict[int, list[dict]]] = defaultdict(lambda: defaultdict(list))
    for path in sorted(findings_dir.glob("findings.*.json")):
        for f in json.loads(path.read_text()):
            if f["variant"] != variant or not f.get("file"): continue
            f["id"] = finding_id(f)
            out[(f["model"], f["run"])][f["pr"]].append(f)
    return out


def code_context(pr: int, path: str, lo: int, hi: int, worktrees: Path, pad: int = 5, cap: int = 120) -> str:
    file = worktrees / f"pr-{pr}" / path
    if not file.is_file(): return "(file not available at the PR head)"
    lines = file.read_text(errors="replace").splitlines()
    start, end = max(1, lo - pad), min(len(lines), hi + pad, max(1, lo - pad) + cap - 1)
    return "\n".join(f"{n:>5} | {lines[n - 1]}" for n in range(start, end + 1))


def render(f: dict) -> str:
    s, e = span(f)
    return f"lines {s}-{e} | {f.get('check_id') or '-'} {f.get('severity') or ''} | {f.get('title') or ''}\n{(f.get('body') or '')[:1200]}"
