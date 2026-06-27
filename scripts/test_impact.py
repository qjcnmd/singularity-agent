#!/usr/bin/env python3
"""Recommend tests to run based on changed source files.

Usage::

    # Explicit file list
    python scripts/test_impact.py src/singularity/context/manager.py src/singularity/planner/engine.py

    # Auto-detect from git diff (staged + unstaged)
    python scripts/test_impact.py --git

    # Auto-detect from git diff against main
    python scripts/test_impact.py --git --base main

The script uses Singularity's built-in ProjectImpactAnalyzer (code index) when
an index exists at ``<workspace>/.singularity/index.sqlite``.  Otherwise it
falls back to simple path-based heuristics.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path, PurePosixPath


def _fallback_tests(paths: list[str]) -> list[str]:
    """Path-based test heuristics when no code index is available.

    Naming conventions in this project (observed from tests/ directory):
      src/singularity/{module}.py          -> tests/test_{module}.py
      src/singularity/{pkg}/{module}.py    -> tests/test_{pkg}.py
                                             tests/test_{pkg}_{module}.py
                                             tests/{pkg}/test_{module}.py
      src/singularity/{pkg}/{subpkg}/*.py  -> tests/{pkg}/test_*.py
    """
    tests: list[str] = []
    for path in paths:
        pure = PurePosixPath(path)
        # Already a test file — include directly
        if "tests" in pure.parts or pure.name.startswith("test_"):
            if Path(path).exists():
                tests.append(path)
            continue
        if pure.suffix != ".py":
            continue
        stem = pure.stem
        parts = list(pure.parts)
        candidates: list[str] = []
        if "singularity" in parts:
            idx = parts.index("singularity")
            pkg_parts = parts[idx + 1:]
            if len(pkg_parts) >= 2:
                pkg = pkg_parts[0]
                # Convention 1: test file named after the package
                candidates.append(f"tests/test_{pkg}.py")
                # Convention 2: test file named after package + module
                candidates.append(f"tests/test_{pkg}_{stem}.py")
                # Convention 3: test in package subdirectory
                candidates.append(f"tests/{pkg}/test_{stem}.py")
                # Convention 4: direct stem match
                candidates.append(f"tests/test_{stem}.py")
            elif len(pkg_parts) == 1:
                candidates.append(f"tests/test_{stem}.py")
        else:
            candidates.append(f"tests/test_{stem}.py")
        for candidate in candidates:
            if Path(candidate).exists():
                tests.append(candidate)
                break
    return sorted(set(tests))


def _git_changed_files(base: str | None) -> list[str]:
    """Get changed files from git diff."""
    if base:
        cmd = ["git", "diff", "--name-only", f"{base}...HEAD"]
    else:
        cmd = ["git", "diff", "--name-only", "HEAD"]
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def _try_code_index(workspace: Path, changed_files: list[str]) -> tuple[list[str], list[str]] | None:
    """Try using the code index for test impact analysis.

    Returns (likely_tests, commands) or None if the index is unavailable.
    """
    db_path = workspace / ".singularity" / "index.sqlite"
    if not db_path.exists():
        return None

    try:
        from singularity.code_index.impact import ProjectImpactAnalyzer
        from singularity.code_index.store import ProjectIndexStore

        store = ProjectIndexStore(db_path)
        analyzer = ProjectImpactAnalyzer(store)
        result = analyzer.get_test_impact(changed_files)
        # Filter to tests that actually exist on disk
        existing = sorted(t for t in result.likely_tests if Path(t).exists())
        # Also apply our richer fallback heuristics
        fallback = _fallback_tests(changed_files)
        combined = sorted(set(existing) | set(fallback))
        commands = []
        if combined:
            commands.append(f"python -m pytest {' '.join(combined)}")
        return combined, commands
    except Exception:
        return None


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Recommend tests based on changed files.",
    )
    parser.add_argument(
        "files",
        nargs="*",
        help="Changed source file paths. Omit if using --git.",
    )
    parser.add_argument(
        "--git",
        action="store_true",
        help="Auto-detect changed files from git diff.",
    )
    parser.add_argument(
        "--base",
        default=None,
        help="Base ref for git diff (e.g. 'main'). Defaults to staged+unstaged.",
    )
    parser.add_argument(
        "--workspace",
        default=".",
        help="Workspace root directory. Defaults to current directory.",
    )
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()

    # Collect changed files
    if args.git:
        changed_files = _git_changed_files(args.base)
        if not changed_files:
            print("No changed files detected.")
            return 0
    elif args.files:
        changed_files = args.files
    else:
        parser.error("Provide file paths or use --git.")
        return 1

    print(f"Changed files ({len(changed_files)}):")
    for f in changed_files:
        print(f"  {f}")
    print()

    # Try code index first
    result = _try_code_index(workspace, changed_files)
    if result is not None:
        likely_tests, commands = result
        source = "code index"
    else:
        likely_tests = _fallback_tests(changed_files)
        commands = [f"python -m pytest {' '.join(likely_tests)}"] if likely_tests else []
        source = "path heuristics (code index not available)"

    print(f"Test mapping source: {source}")
    print(f"Recommended tests ({len(likely_tests)}):")
    for t in likely_tests:
        exists = Path(t).exists()
        status = "" if exists else " (missing)"
        print(f"  {t}{status}")
    print()

    if commands:
        print("Recommended commands:")
        for cmd in commands:
            print(f"  {cmd}")
    else:
        print("No specific tests identified. Run the full suite:")
        print("  python -m pytest")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
