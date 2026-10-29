"""Failed hidden-suite tests per PR, from the spec-bench verifier output in out/tests/pr-N/.

The B9-minichat verifier runs the donor's 556-test e2e suite and maps its JUnit onto a frozen roster
(`expected_tests.txt`): a missing or skipped entry counts as failed. `junit.xml` in each trial's
verifier/ is that mapped result, so the failed entries here add up to 556 - tests_passed.

Writes out/tests/failed.json: {pr: [{"test", "module", "message"}]} and prints how many PRs share
each failing test, which separates one PR's own defects from failures common to most solutions.

Usage: python -m pr_bench.analysis.failed_tests [--tests out/tests]
"""

from __future__ import annotations

import argparse
import json
import re
import xml.etree.ElementTree as ET
from collections import Counter
from pathlib import Path

from pr_bench.paths import OUT


def entry_key(classname: str, name: str) -> str:
    """``<module>::<Class>::<test>`` the same way spec-bench's score.py builds it."""
    parts = classname.split(".")
    module = next((p for p in reversed(parts) if p.startswith("test_")), parts[-1])
    cls = parts[-1] if parts[-1] != module else ""
    return "::".join(x for x in (module, cls, name) if x)


def failed_in(junit: Path) -> list[dict]:
    out = []
    for case in ET.parse(junit).getroot().iter("testcase"):
        bad = next((c for c in case if c.tag in ("failure", "error", "skipped")), None)
        if bad is None: continue
        text = (bad.get("message") or "") + "\n" + (bad.text or "")
        message = re.sub(r"\s+", " ", text).strip()[:600]
        key = entry_key(case.get("classname", ""), case.get("name", ""))
        # the verifier wraps every roster entry as test_roster::test_donor_suite[<donor test id>]
        inner = re.fullmatch(r"test_roster::test_donor_suite\[(.+)\]", key)
        key = inner.group(1) if inner else key
        out.append({"test": key, "module": key.split("::")[0], "kind": bad.tag, "message": message})
    return out


HEADER = re.compile(r"^_{2,} (\S+) _{2,}$")


def failure_blocks(stdout: Path) -> dict[str, str]:
    """``Class.test[param]`` -> the donor pytest's failure section (source line, assert, E-lines).

    junit.xml carries only the roster wrapper's message, so the real assertion comes from here."""
    blocks, current, lines = {}, None, []
    for line in stdout.read_text(errors="replace").splitlines():
        m = HEADER.match(line)
        if m or line.startswith(("=====", "FAILED ", "ERROR ")):
            if current: blocks.setdefault(current, "\n".join(lines[:40]))
            current, lines = (m.group(1) if m else None), []
            continue
        if current: lines.append(line)
    if current: blocks.setdefault(current, "\n".join(lines[:40]))
    return blocks


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--tests", type=Path, default=OUT / "tests")
    args = parser.parse_args()
    result: dict[int, list[dict]] = {}
    for d in sorted(args.tests.glob("pr-*"), key=lambda p: int(p.name[3:])):
        pr = int(d.name[3:])
        fails = failed_in(d / "junit.xml")
        blocks = failure_blocks(d / "pytest-stdout.txt") if (d / "pytest-stdout.txt").exists() else {}
        for f in fails:
            _, cls, name = (f["test"].split("::") + ["", ""])[:3]
            f["detail"] = blocks.get(f"{cls}.{name}", "")
        missing = sum(not f["detail"] for f in fails)
        reward = json.loads((d / "reward.json").read_text())
        expected = reward["tests_total"] - reward["tests_passed"]
        flag = "" if len(fails) == expected else f"  (reward.json says {expected})"
        flag += f"  no assert text for {missing}" if missing else ""
        print(f"#{pr:<4} failed {len(fails):>3}{flag}  modules: {dict(Counter(f['module'] for f in fails).most_common(4))}")
        result[pr] = fails
    (args.tests / "failed.json").write_text(json.dumps(result, indent=1))
    shared = Counter(f["test"] for fails in result.values() for f in fails)
    agent_prs = len([p for p in result if result[p] is not None]) - 1
    print(f"\ndistinct failing tests: {len(shared)}; failing in >= half of the agent PRs: "
          f"{sum(1 for n in shared.values() if n >= agent_prs / 2)}; in exactly one PR: {sum(1 for n in shared.values() if n == 1)}")
    for test, n in shared.most_common(8): print(f"  {n:>2} PRs  {test}")


if __name__ == "__main__":
    main()
