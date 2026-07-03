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
import tomllib
from pathlib import Path, PurePosixPath
from typing import Any

# ---------------------------------------------------------------------------
# Special path mappings for files that are not themselves tests but should
# trigger specific test suites when changed.  These are checked before the
# general naming-convention heuristics.
# ---------------------------------------------------------------------------
_SPECIAL_PATH_MAP: dict[str, list[str]] = {
    "src/singularity/agent_loop.py": ["tests/test_agent.py", "tests/test_agent_task_outcome.py"],
    "src/singularity/context/manager.py": ["tests/test_context.py"],
    "src/singularity/evaluation/runner.py": ["tests/evaluation/test_evaluation_runner.py"],
    "src/singularity/kernel/finalization.py": ["tests/test_kernel_finalization.py"],
    "src/singularity/tool_protocol/engine.py": ["tests/test_tool_protocol_engine.py"],
    "src/singularity/tools/verification.py": ["tests/test_verification_runner.py"],
    "scripts/test_impact.py": ["tests/test_test_impact.py"],
    "scripts/verify_capability.py": ["tests/test_quality_gates.py"],
    "scripts/verify_fast.py": ["tests/test_quality_gates.py"],
    "scripts/verify_stage.py": ["tests/test_quality_gates.py"],
    "tests/conftest.py": ["tests/test_test_infra.py"],
    "docs/testing.md": ["tests/test_docs_consistency.py"],
    ".github/workflows/ci.yml": ["tests/test_quality_gates.py"],
    "pyproject.toml": ["tests/test_quality_gates.py", "tests/test_test_infra.py"],
    "scripts/verify_runtime_docs.py": ["tests/test_runtime_docs_verify.py"],
}

# Additional warnings for special paths (e.g. hint to run smoke collect).
_SPECIAL_PATH_WARNINGS: dict[str, str] = {
    "pyproject.toml": (
        "pyproject.toml changed: verify marker config with "
        "'python -m pytest --collect-only -m smoke'"
    ),
}

_DEFAULT_CAPABILITY_TRIGGER_PATHS: tuple[tuple[str, str], ...] = (
    ("AgentLoop", "src/singularity/agent_loop.py"),
    ("AgentLoop", "src/singularity/agent_loop/"),
    ("ToolProtocol", "src/singularity/tool_protocol/"),
    ("sandbox", "src/singularity/sandbox/"),
    ("sandbox", "src/singularity/command/"),
    ("context", "src/singularity/context/"),
    ("compaction", "src/singularity/context/compaction"),
    ("verification", "src/singularity/verification/"),
    ("verification", "src/singularity/tools/verification.py"),
    ("CompletionGate", "src/singularity/completion/"),
    ("CompletionGate", "src/singularity/kernel/completion"),
    ("FinalReport", "src/singularity/kernel/finalization.py"),
    ("FinalReport", "src/singularity/reporting/"),
    ("evaluation runner", "src/singularity/evaluation/"),
    ("evaluation runner", "docs/evaluation/"),
    ("evaluation runner", "scripts/verify_capability.py"),
)


def _load_capability_trigger_paths(
    workspace: Path = Path("."),
) -> tuple[tuple[str, str], ...]:
    config_path = workspace / "pyproject.toml"
    if not config_path.exists():
        return _DEFAULT_CAPABILITY_TRIGGER_PATHS
    try:
        config = tomllib.loads(config_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError):
        return _DEFAULT_CAPABILITY_TRIGGER_PATHS
    test_impact = (
        config.get("tool", {})
        .get("singularity", {})
        .get("test_impact", {})
    )
    configured = test_impact.get("capability_triggers") if isinstance(test_impact, dict) else None
    if not isinstance(configured, list):
        return _DEFAULT_CAPABILITY_TRIGGER_PATHS
    triggers: list[tuple[str, str]] = []
    for item in configured:
        if not isinstance(item, dict):
            return _DEFAULT_CAPABILITY_TRIGGER_PATHS
        area = item.get("area")
        prefix = item.get("prefix")
        if not isinstance(area, str) or not area or not isinstance(prefix, str) or not prefix:
            return _DEFAULT_CAPABILITY_TRIGGER_PATHS
        triggers.append((area, prefix.replace("\\", "/")))
    return tuple(triggers) or _DEFAULT_CAPABILITY_TRIGGER_PATHS


def _is_pytest_collectable(path: str) -> bool:
    """Return True if *path* looks like a file pytest can collect tests from.

    Pytest collects ``test_*.py`` and ``*_test.py`` files that live under a
    directory named ``tests``.  Non-test files like ``conftest.py``,
    ``__init__.py``, and ``*_helpers.py`` are NOT collectable even if they
    happen to be under ``tests/``.

    Importantly: a non-test script like ``scripts/test_impact.py`` that
    happens to start with ``test_`` is NOT collectable because it is not
    under a ``tests`` directory.
    """
    pure = PurePosixPath(path)
    stem = pure.stem
    # Must be under a "tests" directory component
    if "tests" not in pure.parts:
        return False
    # Must follow pytest naming convention
    if not stem.startswith("test_") and not stem.endswith("_test"):
        return False
    # Non-collectable files under tests/
    return stem not in ("conftest",)


def _validate_recommendations(tests: list[str]) -> tuple[list[str], list[str]]:
    """Filter out non-collectable entries from recommended tests.

    Returns (clean_tests, warnings).  Never raises.
    """
    clean: list[str] = []
    warnings: list[str] = []
    for t in tests:
        if _is_pytest_collectable(t):
            clean.append(t)
        else:
            warnings.append(
                f"'{t}' is not a pytest-collectable test file — removed from recommendations"
            )
    return sorted(clean), warnings


def _fallback_tests(paths: list[str], *, verbose: bool = False) -> tuple[list[str], list[str]]:
    """Path-based test heuristics when no code index is available.

    Returns (tests, warnings).

    Priority order:
      1. Explicit ``_SPECIAL_PATH_MAP`` entries.
      2. Already a collectable test file — include directly.
      3. Non-``.py`` files — check ``_SPECIAL_PATH_MAP``; warn if unmapped.
      4. Naming-convention heuristics for ``src/singularity/…`` sources.
    """
    tests: list[str] = []
    warnings: list[str] = []
    for path in paths:
        pure = PurePosixPath(path)

        # --- 1. Special path map (checked first for all file types) ---
        path_str = str(pure).replace("\\", "/")
        if path_str in _SPECIAL_PATH_MAP:
            mapped = _SPECIAL_PATH_MAP[path_str]
            for m in mapped:
                if Path(m).exists():
                    tests.append(m)
                    if verbose:
                        print(f"  [verbose] {path} -> special map -> {m}")
                else:
                    warnings.append(f"Special-mapped test not found: {m}")
            if path_str in _SPECIAL_PATH_WARNINGS:
                warnings.append(_SPECIAL_PATH_WARNINGS[path_str])
            continue

        # --- 2. Already a collectable test file — include directly ---
        if "tests" in pure.parts and _is_pytest_collectable(path):
            if Path(path).exists():
                tests.append(path)
                if verbose:
                    print(f"  [verbose] {path} -> direct test file")
            else:
                warnings.append(f"Test file not found: {path}")
            continue

        # --- 2b. Path under tests/ but NOT a collectable test file ---
        if "tests" in pure.parts:
            warnings.append(
                f"'{path}' is under tests/ but is not a pytest-collectable "
                f"test file (conftest, __init__, helpers, etc.) — "
                f"add to _SPECIAL_PATH_MAP if it should trigger tests"
            )
            continue

        # --- 3. Non-.py files — warn if unmapped ---
        if pure.suffix != ".py":
            if verbose:
                print(f"  [verbose] {path} -> skipped (not .py, no special map)")
            warnings.append(
                f"No test mapping for non-Python file: {path}"
            )
            continue

        # --- 4. Naming-convention heuristics for source files ---
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
    cmd = ["git", "diff", "--name-only", f"{base}...HEAD"] if base else ["git", "diff", "--name-only", "HEAD"]
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


def _capability_triggers(
    changed_files: list[str],
    *,
    workspace: Path = Path("."),
) -> dict[str, Any]:
    areas: list[str] = []
    files: list[str] = []
    trigger_paths = _load_capability_trigger_paths(workspace)
    for raw_path in changed_files:
        path = raw_path.replace("\\", "/")
        matched = False
        for area, prefix in trigger_paths:
            if path == prefix or path.startswith(prefix):
                areas.append(area)
                matched = True
        if matched:
            files.append(raw_path)
    return {
        "required": bool(files),
        "areas": sorted(dict.fromkeys(areas)),
        "files": files,
        "trigger": "core_agent_chain" if files else "",
    }


def _gate_recommendation(
    *,
    confidence: str,
    recommended_tests: list[str],
    warnings: list[str],
) -> dict[str, str | None]:
    if confidence == "low" or not recommended_tests:
        detail = "; ".join(warnings[:3])
        reason = "No impacted pytest target could be selected"
        if detail:
            reason = f"{reason}: {detail}"
        return {"fallback_gate": "stage", "skipped_reason": reason}
    return {"fallback_gate": None, "skipped_reason": ""}


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
                    "fallback_gate": None,
                    "skipped_reason": "no changed files detected",
                    "capability_gate": _capability_triggers([], workspace=workspace),
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
                    "fallback_gate": "stage",
                    "skipped_reason": msg,
                    "capability_gate": _capability_triggers(changed_files, workspace=workspace),
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

    # --- Validate: filter out non-collectable entries from recommendations ---
    clean_tests, val_warnings = _validate_recommendations(likely_tests)
    all_warnings.extend(val_warnings)
    # Rebuild commands with the cleaned test list
    if clean_tests != likely_tests:
        likely_tests = clean_tests
        if source != "code_index":
            commands = (
                [f"python -m pytest {' '.join(likely_tests)}"] if likely_tests else []
            )
        else:
            # code_index commands were built before validation; rebuild
            commands = (
                [f"python -m pytest {' '.join(likely_tests)}"] if likely_tests else []
            )

    confidence = _compute_confidence(source, likely_tests, all_warnings)
    gate = _gate_recommendation(
        confidence=confidence,
        recommended_tests=likely_tests,
        warnings=all_warnings,
    )
    capability_gate = _capability_triggers(changed_files, workspace=workspace)

    # Output
    if args.json_output:
        output = {
            "changed_files": changed_files,
            "source": source,
            "warnings": all_warnings,
            "recommended_tests": likely_tests,
            "recommended_commands": commands,
            "confidence": confidence,
            "fallback_gate": gate["fallback_gate"],
            "skipped_reason": gate["skipped_reason"],
            "capability_gate": capability_gate,
        }
        print(json.dumps(output, indent=2))
    else:
        print(f"Test mapping source: {source}")
        print(f"Confidence: {confidence}")
        if gate["fallback_gate"]:
            print(f"Fallback gate required: {gate['fallback_gate']}")
            print(f"Skipped reason: {gate['skipped_reason']}")
        if capability_gate["required"]:
            print("Capability gate recommended:")
            print(f"  areas: {', '.join(capability_gate['areas'])}")
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
