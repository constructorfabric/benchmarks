#!/usr/bin/env python3
"""
Sort OpenAPI JSON file to ensure deterministic ordering.

This script reads an OpenAPI JSON file, sorts all keys recursively,
and writes it back with consistent formatting.
"""

import json
import sys
from pathlib import Path


def sort_dict_recursive(obj):
    """
    Recursively sort all dictionaries in the object.

    Args:
        obj: The object to sort (can be dict, list, or primitive)

    Returns:
        The sorted object
    """
    if isinstance(obj, dict):
        return {k: sort_dict_recursive(v) for k, v in sorted(obj.items())}
    elif isinstance(obj, list):
        return [sort_dict_recursive(item) for item in obj]
    else:
        return obj


def main():
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <openapi.json>", file=sys.stderr)
        sys.exit(1)

    file_path = Path(sys.argv[1])

    if not file_path.exists():
        print(f"Error: File '{file_path}' does not exist", file=sys.stderr)
        sys.exit(1)

    # Read the JSON file
    try:
        with open(file_path, 'r', encoding='utf-8') as f:
            data = json.load(f)
    except json.JSONDecodeError as e:
        print(f"Error: Failed to parse JSON: {e}", file=sys.stderr)
        sys.exit(1)

    # Sort all keys recursively
    sorted_data = sort_dict_recursive(data)

    # Write back with consistent formatting
    with open(file_path, 'w', encoding='utf-8') as f:
        json.dump(sorted_data, f, indent=2, ensure_ascii=False)
        f.write('\n')  # Add trailing newline

    print(f"Successfully sorted {file_path}")


if __name__ == '__main__':
    main()
