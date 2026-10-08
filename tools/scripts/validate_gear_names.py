#!/usr/bin/env python3
"""
Validate gear folder names follow kebab-case naming convention.

This script ensures all gear directory names in gears/ follow the same
kebab-case rules enforced by the #[toolkit::gear] macro at compile time:
- Must contain only lowercase letters (a-z), digits (0-9), and hyphens (-)
- Must start with a lowercase letter
- Must not end with a hyphen
- Must not contain consecutive hyphens
- Must not contain underscores (use hyphens instead)

Exit codes:
  0 - All gear names are valid
  1 - One or more gear names violate kebab-case rules
"""

import sys
from pathlib import Path
from typing import List, Tuple


def validate_kebab_case(name: str) -> Tuple[bool, str]:
    """
    Validate that a gear name follows kebab-case convention.

    Returns:
        (is_valid, error_message)
    """
    if not name:
        return False, "gear name cannot be empty"

    # Check for underscores (common mistake - should be kebab-case, not snake_case)
    if "_" in name:
        suggested = name.replace("_", "-")
        return False, f"gear name must use kebab-case, not snake_case\n       → use '{suggested}' instead of '{name}'"

    # Must start with a lowercase letter
    if not name[0].islower() or not name[0].isalpha():
        return False, f"gear name must start with a lowercase letter, found '{name[0]}'"

    # Must not end with hyphen
    if name.endswith("-"):
        return False, "gear name must not end with a hyphen"

    # Check for invalid characters and consecutive hyphens
    prev_was_hyphen = False
    for ch in name:
        if ch == "-":
            if prev_was_hyphen:
                return False, "gear name must not contain consecutive hyphens"
            prev_was_hyphen = True
        elif ch.islower() or ch.isdigit():
            prev_was_hyphen = False
        else:
            return False, f"gear name must contain only lowercase letters, digits, and hyphens, found '{ch}'"

    return True, ""


def find_gears(gears_dir: Path) -> List[Path]:
    """Find all gear directories (direct subdirectories of gears/)."""
    if not gears_dir.exists() or not gears_dir.is_dir():
        return []

    gears = []
    for item in gears_dir.iterdir():
        if item.is_dir() and not item.name.startswith("."):
            gears.append(item)

    return sorted(gears)


def main() -> int:
    # Find workspace root (script is in tools/scripts/, workspace root is grandparent)
    script_dir = Path(__file__).parent
    workspace_root = script_dir.parent.parent
    gears_dir = workspace_root / "gears"

    if not gears_dir.exists():
        print(f"Error: gears/ directory not found at {gears_dir}", file=sys.stderr)
        return 1

    # Find all gear directories
    gears = find_gears(gears_dir)

    if not gears:
        print("Warning: No gears found in gears/", file=sys.stderr)
        return 0

    # Validate each gear name
    violations = []
    valid_count = 0

    for gear_path in gears:
        gear_name = gear_path.name
        is_valid, error_msg = validate_kebab_case(gear_name)

        if not is_valid:
            violations.append((gear_path, error_msg))
        else:
            valid_count += 1

    # Report results
    if violations:
        print("=" * 80, file=sys.stderr)
        print("MODULE NAMING VIOLATIONS DETECTED", file=sys.stderr)
        print("=" * 80, file=sys.stderr)
        print(file=sys.stderr)
        print("All gear folder names must follow kebab-case convention:", file=sys.stderr)
        print("  - Lowercase letters (a-z), digits (0-9), and hyphens (-) only", file=sys.stderr)
        print("  - Must start with a lowercase letter", file=sys.stderr)
        print("  - No trailing hyphens or consecutive hyphens", file=sys.stderr)
        print("  - No underscores (use hyphens instead)", file=sys.stderr)
        print(file=sys.stderr)
        print(f"Found {len(violations)} violation(s):", file=sys.stderr)
        print(file=sys.stderr)

        for gear_path, error_msg in violations:
            rel_path = gear_path.relative_to(workspace_root)
            print(f"  [X] {rel_path}/", file=sys.stderr)
            print(f"      {error_msg}", file=sys.stderr)
            print(file=sys.stderr)

        print("=" * 80, file=sys.stderr)
        print(f"Summary: {valid_count} valid, {len(violations)} invalid", file=sys.stderr)
        print("=" * 80, file=sys.stderr)
        return 1

    # All valid
    print(f"OK: All {valid_count} gear names follow kebab-case convention")
    return 0


if __name__ == "__main__":
    sys.exit(main())
