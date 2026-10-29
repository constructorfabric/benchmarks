"""Assemble PR-Agent `extra_instructions` texts from docs/toolkit-pr-review/.

Two variants:
  per-module  one file per rule module (+ the architecture pass), mirroring the
              gears-rust toolkit-pr-review skill: an agent holds one module only.
  single      every module in one prompt.

Usage: python -m pr_bench.runner.build_instructions [--out instructions/]
"""

from __future__ import annotations

import argparse
from pathlib import Path

from pr_bench.paths import INSTRUCTIONS, RULES_DIR

MODULES = ("errors", "security", "async", "design", "tests", "toolkit")

OUTPUT_MAPPING = """\
# How to report

You are one reviewer in a Rust code review pipeline. The rule modules below are
the only criteria you apply; the shared conventions and comment style govern how a
finding is worded and what severity it gets.

Map every finding onto the output schema like this. The check ID is mandatory and it
overrides the generic header examples elsewhere in this prompt ("Possible Bug" and the
like are NOT acceptable headers here):
- `issue_header`: MUST start with the check ID from the rule module, then ` - `, then a
  two-to-five word title. Example: `RUST-ERR-001 - unwrap on fallible parse`. Never put a
  colon in this field: an unquoted `ID: title` breaks the YAML.
- `issue_content`: MUST start with `[<check ID>][<severity>] `, for example
  `[RUST-ERR-001][HIGH] `, followed by the finding as the comment-style guide prescribes,
  including the criterion marker it asks for. Do not repeat the file path or the line
  numbers here.
- A finding you cannot attribute to a check ID of your module is out of scope: drop it.
- `relevant_file`, `start_line`, `end_line`: the added lines the finding anchors to.
- Report every criterion violation you can substantiate from the diff. A second and
  third finding in the same file is expected. Do not stop at a round number.
- Do not report anything a rule module does not cover. Do not invent check IDs.
"""

GO_DEEP = """\
You hold one rule module and nothing else, so go deep: walk the changed files one
at a time and apply every criterion of your module to each of them.
"""


def read(name: str) -> str:
    return (RULES_DIR / name).read_text()


def section(title: str, body: str) -> str:
    return f"\n\n<!-- {title} -->\n{body.strip()}\n"


def shared_tail() -> str:
    return section("review-conventions.md", read("review-conventions.md")) + section(
        "comment-style.md", read("comment-style.md")
    )


def build_module(module: str) -> str:
    return (
        OUTPUT_MAPPING
        + GO_DEEP
        + section("agents/subject.md", read("agents/subject.md"))
        + section(f"rules/{module}.md", read(f"rules/{module}.md"))
        + shared_tail()
    )


def build_architecture() -> str:
    return (
        OUTPUT_MAPPING
        + section("agents/architecture.md", read("agents/architecture.md"))
        + shared_tail()
    )


def build_single() -> str:
    modules = "".join(section(f"rules/{m}.md", read(f"rules/{m}.md")) for m in MODULES)
    return (
        OUTPUT_MAPPING
        + section("agents/subject.md", read("agents/subject.md"))
        + modules
        + section("agents/architecture.md", read("agents/architecture.md"))
        + shared_tail()
    )


def write_all(out_dir: Path) -> list[Path]:
    per_module = out_dir / "per-module"
    per_module.mkdir(parents=True, exist_ok=True)
    (out_dir / "single").mkdir(parents=True, exist_ok=True)
    written = []
    for module in MODULES:
        path = per_module / f"{module}.md"
        path.write_text(build_module(module))
        written.append(path)
    path = per_module / "architecture.md"
    path.write_text(build_architecture())
    written.append(path)
    path = out_dir / "single" / "all.md"
    path.write_text(build_single())
    written.append(path)
    return written


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, default=INSTRUCTIONS)
    args = parser.parse_args()
    for path in write_all(args.out):
        print(f"{path.relative_to(args.out.parent)}  {path.stat().st_size // 1024} KB")


if __name__ == "__main__":
    main()
