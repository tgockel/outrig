#!/usr/bin/env python3
"""Doc-style auditing.

Walks Markdown under a target path and enforces three rules:

  Width    -- each line is at most 100 code points (multi-byte chars count as one).
  Links    -- relative-path links of the form `](./foo)` or `](../foo)` resolve to
              an existing file or directory on disk. Absolute URLs and bare-anchor
              links (`(#section)`) are intentionally skipped.
  Spelling -- no British-English variants (the project uses American English).

Exit code 0 on success, 1 on any violation. Violations print to stdout under a
per-check heading. Standard library only; no `pip install` step needed in CI.
"""

from __future__ import annotations

import argparse
import os
import re
import sys
from collections.abc import Iterable

MAX_WIDTH = 100

LINK_RE = re.compile(r"\]\((\.\.?/?[^)#]*)(?:#[^)]*)?\)")

BRITISH_RE = re.compile(
    r"\b("
    r"behaviour|"
    r"optimi[s]e|optimi[s]ation|optimiser|"
    r"recogni[s]e|"
    r"analyse|"
    r"colour|favour|centre|"
    r"defence|licence|"
    r"realise|specialise|organise"
    r")\b",
    re.IGNORECASE,
)


def iter_markdown(path: str) -> Iterable[str]:
    if os.path.isfile(path):
        yield path
        return
    for dirpath, _, filenames in os.walk(path):
        for name in filenames:
            if name.endswith(".md"):
                yield os.path.join(dirpath, name)


def check_per_line(files: Iterable[str], width_only: bool) -> tuple[list[str], list[str]]:
    """Single pass over each file's lines: width check (always) plus spelling (unless width-only)."""
    width_violations: list[str] = []
    spelling_violations: list[str] = []
    for path in files:
        with open(path, encoding="utf-8") as fh:
            for lineno, line in enumerate(fh, 1):
                trimmed = line.rstrip("\n")
                if len(trimmed) > MAX_WIDTH:
                    width_violations.append(f"{path}:{lineno}  cols={len(trimmed)}")
                if not width_only:
                    match = BRITISH_RE.search(trimmed)
                    if match:
                        spelling_violations.append(f"{path}:{lineno}: {match.group(0)!r}")
    return width_violations, spelling_violations


def check_links(files: Iterable[str]) -> list[str]:
    violations: list[str] = []
    for path in files:
        dirpath = os.path.dirname(path)
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
        for match in LINK_RE.finditer(text):
            target = match.group(1)
            resolved = os.path.normpath(os.path.join(dirpath, target))
            if not os.path.exists(resolved):
                violations.append(f"{path}: broken link to {target}")
    return violations


def report(label: str, violations: list[str]) -> bool:
    if violations:
        print(f"{label}:")
        for v in violations:
            print(f"  {v}")
        return False
    print(f"{label}: OK")
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--width-only",
        action="store_true",
        help="only run the line-width check",
    )
    parser.add_argument(
        "path",
        nargs="?",
        default="doc",
        help="file or directory to audit (default: doc)",
    )
    args = parser.parse_args()

    files = list(iter_markdown(args.path))
    width, spelling = check_per_line(files, args.width_only)
    ok = report("Width", width)
    if not args.width_only:
        ok &= report("Links", check_links(files))
        ok &= report("Spelling", spelling)

    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
