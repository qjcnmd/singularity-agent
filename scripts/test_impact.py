#!/usr/bin/env python3
"""Recommend tests to run based on changed source files.

Usage::

    # Explicit file list
    python scripts/test_impact.py src/singularity/context/manager.py src/singularity/planner/engine.py

    # Auto-detect from git diff (staged + unstaged)
    python scripts/test_impact.py --git

    # Auto-detect from git diff against main
    python scripts/test_impact.py --git --base main

    # JSON output
    python scripts/test_impact.py --git --json

    # Verbose mode
    python scripts/test_impact.py --git --verbose

    # Require code index (fail if unavailable)
    python scripts/test_impact.py --git --strict-index

The script uses Singularity's built-in ProjectImpactAnalyzer (code index) when
an index exists at ``<workspace>/.singularity/index.sqlite``.  Otherwise it
falls back to simple path-based heuristics.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path, PurePosixPath


def _fallback_tests(paths: list[str], *, verbose: bool = False) -> tuple[list[str], list[str]]:
    """Path-based test heuristics when no code index is available.

    Returns (tests, warnings).

    Naming conventions in this project (observed from tests/ directory):
      src/singularity/{module}.py          -> tests/test_{module}.py
      src/singularity/{pkg}/{module}.py    -> tests/test_{pkg}.py
                                             tests/test_{pkg}_{module}.py
                                             tests/{pkg}/test_{module}.py
      src/singularity/{pkg}/{subpkg}/*.py  -> tests/{pkg}/test_*.py
    """
    tests: list[str] = []
    warnings: list[str] = []
    for path in paths:
        pure = PurePosixPath(path)
        # Already a test file — include directly
        if "tests" in pure.parts or pure.name.startswith("test_"):
            if Path(path).exists():
                tests.append(path)
                if verbose:
                    print(f"  [verbose] {path} -> direct test file")
            else:
                warnings.append(f"Test file not found: {path}")
            continue
        if pure.suffix != ".py":
            if verbose:
                print(f"  [verbose] {path} -> skipped (not .py)")
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
        matched = False
        for candidate in candidates:
            if Path(candidate).exists():
                tests.append(candidate)
                matched = True
                if verbose:
                    print(f"  [verbose] {path} -> {candidate}")
                break
        if not matched and verbose:
            print(f"  [verbose] {path} -> no test found (tried: {candidates})")
    return sorted(set(tests)), warnings


def _git_changed_files(base: str | None) -> list[str]:
    """Get changed files from git diff."""
    if base:
        cmd = ["git", "diff", "--name-only", f"{base}...HEAD"]
    else:
        cmd = ["git", "diff", "--name-only", "HEAD"]
    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return [line.strip() for line in result.stdout.splitlines() if line.strip()]


def _try_code_index(
    workspace: Path,
    changed_files: list[str],
    *,
    verbose: bool = False,
) -> tuple[list[str], list[str], list[str]] | None:
    """Try using the code index for test impact analysis.

    Returns (likely_tests, commands, warnings) or None if the index is
    unavailable.  Raises on non-index-related errors so callers can surface
    them instead of silently falling back.
    """
    db_path = workspace / ".singularity" / "index.sqlite"
    if not db_path.exists():
        if verbose:
            print(f"  [verbose] Code index not found at {db_path}")
        return None

    warnings: list[str] = []
    try:
        from singularity.code_index.impact import ProjectImpactAnalyzer
        from singularity.code_index.store import ProjectIndexStore
    except ImportError as exc:
        # Missing dependency — cannot use code index at all.
        warnings.append(f"Code index import failed: {exc}")
        return None

    try:
        store = ProjectIndexStore(db_path)
        analyzer = ProjectImpactAnalyzer(store)
        result = analyzer.get_test_impact(changed_files)
        # Filter to tests that actually exist on disk
        existing = sorted(t for t in result.likely_tests if Path(t).exists())
        missing = sorted(t for t in result.likely_tests if not Path(t).exists())
        if missing:
            warnings.append(
                f"Code index returned {len(missing)} test(s) not found on disk: "
                + ", ".join(missing[:5])
                + ("..." if len(missing) > 5 else "")
            )
        if verbose:
            print(f"  [verbose] Code index returned {len(result.likely_tests)} tests, "
                  f"{len(existing)} exist on disk")
        # Also apply our richer fallback heuristics
        fallback, fb_warnings = _fallback_tests(changed_files, verbose=verbose)
        warnings.extend(fb_warnings)
        combined = sorted(set(existing) | set(fallback))
        commands = []
        if combined:
            commands.append(f"python -m pytest {' '.join(combined)}")
        return combined, commands, warnings
    except Exception as exc:
        # Only catch code-index-specific errors (sqlite, analysis).
        # Re-raise unexpected errors so they are not swallowed.
        warnings.append(f"Code index analysis failed: {exc}")
        if verbose:
            print(f"  [verbose] Code index analysis error: {exc}")
        # Fall through to heuristic fallback
        return None


def _compute_confidence(
    source: str,
    recommended_tests: list[str],
    warnings: list[str],
) -> str:
    """Compute confidence level for the recommendation."""
    if source.startswith("code_index"):
        return "high" if recommended_tests else "medium"
    # path heuristics
    if recommended_tests:
        return "medium"
    return "low"


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
    parser.add_argument(
        "--json",
        action="store_true",
        dest="json_output",
        help="Output results as JSON.",
    )
    parser.add_argument(
        "--verbose",
        action="store_true",
        help="Show detailed analysis process.",
    )
    parser.add_argument(
        "--strict-index",
        action="store_true",
        help="Require code index to be available. Exit 1 if not.",
    )
    args = parser.parse_args()

    workspace = Path(args.workspace).resolve()

    # Collect changed files
    if args.git:
        changed_files = _git_changed_files(args.base)
        if not changed_files:
            if args.json_output:
                print(json.dumps({
                    "changed_files": [],
                    "source": "none",
                    "warnings": [],
                    "recommended_tests": [],
                    "recommended_commands": [],
                    "confidence": "low",
                }, indent=2))
            else:
                print("No changed files detected.")
            return 0
    elif args.files:
        changed_files = args.files
    else:
        parser.error("Provide file paths or use --git.")
        return 1

    all_warnings: list[str] = []

    if not args.json_output:
        print(f"Changed files ({len(changed_files)}):")
        for f in changed_files:
            print(f"  {f}")
        print()

    # Try code index first
    result = _try_code_index(workspace, changed_files, verbose=args.verbose)
    if result is not None:
        likely_tests, commands, idx_warnings = result
        all_warnings.extend(idx_warnings)
        source = "code_index"
    else:
        # Check strict-index requirement
        if args.strict_index:
            db_path = workspace / ".singularity" / "index.sqlite"
            msg = f"Code index not available at {db_path}"
            all_warnings.append(msg)
            if args.json_output:
                print(json.dumps({
                    "changed_files": changed_files,
                    "source": "unavailable",
                    "warnings": all_warnings,
                    "recommended_tests": [],
                    "recommended_commands": [],
                    "confidence": "low",
                }, indent=2))
            else:
                print(f"ERROR: {msg}", file=sys.stderr)
            return 1

        likely_tests, fb_warnings = _fallback_tests(
            changed_files, verbose=args.verbose,
        )
        all_warnings.extend(fb_warnings)
        commands = (
            [f"python -m pytest {' '.join(likely_tests)}"] if likely_tests else []
        )
        source = "path_heuristics"

    confidence = _compute_confidence(source, likely_tests, all_warnings)

    # Output
    if args.json_output:
        output = {
            "changed_files": changed_files,
            "source": source,
            "warnings": all_warnings,
            "recommended_tests": likely_tests,
            "recommended_commands": commands,
            "confidence": confidence,
        }
        print(json.dumps(output, indent=2))
    else:
        print(f"Test mapping source: {source}")
        print(f"Confidence: {confidence}")
        if all_warnings:
            print(f"\nWarnings ({len(all_warnings)}):")
            for w in all_warnings:
                print(f"  ⚠ {w}")
        print(f"\nRecommended tests ({len(likely_tests)}):")
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
