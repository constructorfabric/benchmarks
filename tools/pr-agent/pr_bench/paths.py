"""Locations shared by the runner and the analysis. Generated data lives under out/ (gitignored)."""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = ROOT.parents[1]
OUT = ROOT / "out"
WORKTREES = OUT / "worktrees"
INSTRUCTIONS = ROOT / "instructions"
PR_AGENT_TOML = ROOT / "pr_agent.toml"
RULES_DIR = REPO_ROOT / "docs" / "toolkit-pr-review"
