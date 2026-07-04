"""Self-check tests for the test infrastructure.

Validates that curated lists (smoke/flaky/slow/external) are consistent
with the actual test collection, and that marker assignments are correct.
These tests are classified as ``regression`` because they guard the
reliability of the test suite itself.

NOTE: Curated list integrity tests require the full test collection.
When running only this file or a subset, nodeid-based checks are skipped.
Run ``python -m pytest tests/test_test_infra.py -m "not evaluation and not provider_eval"``
for complete validation including slow/external lists.
"""

from __future__ import annotations

import tomllib
from pathlib import Path

import pytest

from tests.conftest import (
    _EXTERNAL_FILE_STEMS,
    _FLAKY_TEST_IDS,
    _INTEGRATION_FILE_STEMS,
    _KNOWN_MARKERS,
    _SLOW_TEST_IDS,
    _SMOKE_FILE_STEMS,
    _SMOKE_TEST_IDS,
    _is_full_suite_collection,
)

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_FULL_SUITE_THRESHOLD = 100  # minimum items to consider collection complete
_MARKER_COUNT_CONFIG = (
    Path(__file__).resolve().parents[1]
    / "pyproject.toml"
)


def _is_full_collection(request: pytest.FixtureRequest) -> bool:
    """Check if the current collection represents the full test suite."""
    return _is_full_suite_collection(request.config, list(request.session.items))


def _collect_nodeids_and_stems(request: pytest.FixtureRequest):
    """Return (all_nodeids, all_file_stems) from the session collection."""
    session = request.session
    nodeids = {item.nodeid for item in session.items}
    stems = {Path(item.fspath).stem for item in session.items}
    return nodeids, stems


def _is_filter_active(pytestconfig: pytest.Config, *markers: str) -> bool:
    """Check if the addopts or -m flag filters exclude specific markers."""
    # Check both addopts and override-ini for -m filters
    sources = []
    addopts = pytestconfig.getini("addopts")
    if isinstance(addopts, list):
        sources.append(" ".join(addopts))
    else:
        sources.append(str(addopts))
    # Also check command-line -m override
    override = pytestconfig.getoption("markexpr", default="")
    if override:
        sources.append(override)
    combined = " ".join(sources)
    return any(f"not {marker}" in combined for marker in markers)


def _configured_marker_counts() -> tuple[dict[str, int], float]:
    payload = tomllib.loads(_MARKER_COUNT_CONFIG.read_text(encoding="utf-8"))
    marker_counts = (
        payload.get("tool", {})
        .get("singularity", {})
        .get("test_infra", {})
        .get("marker_counts", {})
    )
    expected = {
        str(marker): int(count)
        for marker, count in marker_counts.items()
        if marker != "tolerance"
    }
    tolerance = float(marker_counts.get("tolerance", 0.3))
    return expected, tolerance


# ---------------------------------------------------------------------------
# Curated list existence checks
# ---------------------------------------------------------------------------

class TestCuratedListIntegrity:
    """Verify that every test ID in curated lists actually exists.

    These tests only run when the full suite is collected (>=100 items).
    Lists filtered by default addopts (slow, external) are skipped when
    those markers are excluded.
    """

    def test_smoke_test_ids_all_exist(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        # Smoke tests now carry diverse functional markers (unit, integration,
        # etc.).  When a marker filter is active, smoke tests with non-matching
        # functional markers are deselected.
        override = pytestconfig.getoption("markexpr", default="")
        if override:
            pytest.skip(
                f"Marker filter '-m {override}' active; "
                f"smoke tests may be deselected"
            )
        nodeids, _ = _collect_nodeids_and_stems(request)
        missing = _SMOKE_TEST_IDS - nodeids
        assert not missing, (
            f"Smoke test IDs not found in collection: {sorted(missing)}"
        )

    def test_smoke_file_stems_all_exist(self, request: pytest.FixtureRequest) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        _, stems = _collect_nodeids_and_stems(request)
        missing = _SMOKE_FILE_STEMS - stems
        assert not missing, (
            f"Smoke file stems not found in collection: {sorted(missing)}"
        )

    def test_flaky_test_ids_all_exist(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        # Flaky tests have diverse markers (security, integration, unit).
        # When running with a specific marker filter (e.g. -m unit),
        # flaky tests with other markers will be deselected.
        # Check if any marker filter beyond the default addopts is active.
        override = pytestconfig.getoption("markexpr", default="")
        if override:
            pytest.skip(
                f"Marker filter '-m {override}' active; "
                f"flaky tests may be deselected"
            )
        nodeids, _ = _collect_nodeids_and_stems(request)
        missing = _FLAKY_TEST_IDS - nodeids
        assert not missing, (
            f"Flaky test IDs not found in collection: {sorted(missing)}"
        )

    def test_slow_test_ids_all_exist(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        if _is_filter_active(pytestconfig, "slow"):
            pytest.skip("slow tests excluded by default addopts")
        nodeids, _ = _collect_nodeids_and_stems(request)
        missing = _SLOW_TEST_IDS - nodeids
        assert not missing, (
            f"Slow test IDs not found in collection: {sorted(missing)}"
        )

    def test_external_file_stems_all_exist(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        if _is_filter_active(pytestconfig, "external"):
            pytest.skip("external tests excluded by default addopts")
        _, stems = _collect_nodeids_and_stems(request)
        missing = _EXTERNAL_FILE_STEMS - stems
        assert not missing, (
            f"External file stems not found in collection: {sorted(missing)}"
        )

    def test_large_single_file_collection_is_not_full_suite(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        if len(request.session.items) < _FULL_SUITE_THRESHOLD:
            pytest.skip("Requires a large collected file")

        stems = {Path(item.fspath).stem for item in request.session.items}
        if len(stems) != 1:
            pytest.skip("Only exercises single-file subset collections")

        assert not _is_full_suite_collection(request.config, list(request.session.items))

    def test_windows_sandbox_backend_file_is_not_implicitly_external(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        selected = [
            item
            for item in request.session.items
            if Path(item.fspath).stem == "test_sandbox_backend_windows"
        ]
        if not selected:
            pytest.skip("Windows sandbox backend tests not collected")

        external = [item.nodeid for item in selected if "external" in {m.name for m in item.iter_markers()}]
        assert not external, (
            "Windows sandbox backend contract tests should not be excluded "
            f"as external by file/name heuristics: {external[:5]}"
        )


# ---------------------------------------------------------------------------
# Marker overlap checks (always safe — no dependency on collection size)
# ---------------------------------------------------------------------------

class TestMarkerConsistency:
    """Verify marker assignments are mutually consistent."""

    def test_smoke_not_in_slow(self) -> None:
        overlap = _SMOKE_TEST_IDS & _SLOW_TEST_IDS
        assert not overlap, (
            f"Tests in both smoke and slow: {sorted(overlap)}"
        )

    def test_smoke_not_in_flaky(self) -> None:
        overlap = _SMOKE_TEST_IDS & _FLAKY_TEST_IDS
        assert not overlap, (
            f"Tests in both smoke and flaky: {sorted(overlap)}"
        )

    def test_smoke_files_not_in_external(self) -> None:
        overlap = _SMOKE_FILE_STEMS & _EXTERNAL_FILE_STEMS
        assert not overlap, (
            f"Files in both smoke and external: {sorted(overlap)}"
        )


# ---------------------------------------------------------------------------
# Functional classification checks
# ---------------------------------------------------------------------------

_FUNCTIONAL_MARKERS = {"unit", "integration", "regression", "security", "evaluation", "provider_eval"}


class TestFunctionalClassification:
    """Verify slow/external tests have correct non-unit functional classification."""

    def test_slow_tests_have_non_unit_functional_marker(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        """Every test in _SLOW_TEST_IDS must have a functional marker other than unit."""
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        if _is_filter_active(pytestconfig, "slow"):
            pytest.skip("slow tests excluded by default addopts")

        _FUNCTIONAL = {"unit", "integration", "regression", "security", "evaluation", "provider_eval"}
        violations: list[str] = []
        for item in request.session.items:
            if item.nodeid not in _SLOW_TEST_IDS:
                continue
            item_markers = {m.name for m in item.iter_markers()}
            functional = item_markers & _FUNCTIONAL
            if "unit" in functional or not functional:
                violations.append(
                    f"{item.nodeid} has functional={sorted(functional) if functional else 'NONE'}"
                )
        assert not violations, (
            "Slow tests with unit or no functional marker:\n  "
            + "\n  ".join(sorted(violations))
        )

    def test_external_tests_have_non_unit_functional_marker(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        """Every external test must have a functional marker other than unit."""
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        if _is_filter_active(pytestconfig, "external"):
            pytest.skip("external tests excluded by default addopts")

        violations: list[str] = []
        for item in request.session.items:
            item_markers = {m.name for m in item.iter_markers()}
            if "external" not in item_markers:
                continue
            functional = item_markers & _FUNCTIONAL_MARKERS
            if "unit" in functional or not functional:
                violations.append(
                    f"{item.nodeid} has functional={sorted(functional) if functional else 'NONE'}"
                )
        assert not violations, (
            "External tests with unit or no functional marker:\n  "
            + "\n  ".join(sorted(violations))
        )

    def test_integration_file_stems_all_exist(
        self,
        request: pytest.FixtureRequest,
        pytestconfig: pytest.Config,
    ) -> None:
        """All stems in _INTEGRATION_FILE_STEMS must exist in collection."""
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
        # When a marker filter is active, integration files may be deselected.
        override = pytestconfig.getoption("markexpr", default="")
        if override:
            pytest.skip(
                f"Marker filter '-m {override}' active; "
                f"integration files may be deselected"
            )
        _, stems = _collect_nodeids_and_stems(request)
        missing = _INTEGRATION_FILE_STEMS - stems
        assert not missing, (
            f"Integration file stems not found in collection: {sorted(missing)}"
        )

    def test_smoke_not_in_provider_eval(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        """No test should have both smoke and provider_eval markers."""
        overlap: list[str] = []
        for item in request.session.items:
            item_markers = {m.name for m in item.iter_markers()}
            if "smoke" in item_markers and "provider_eval" in item_markers:
                overlap.append(item.nodeid)
        assert not overlap, (
            f"Tests in both smoke and provider_eval: {sorted(overlap)}"
        )


# ---------------------------------------------------------------------------
# Marker validity checks
# ---------------------------------------------------------------------------

# Plugin-provided markers that are not in _KNOWN_MARKERS but are valid.
_PLUGIN_MARKERS = {
    "parametrize",
    "skip",
    "skipif",
    "xfail",
    "usefixtures",
    "filterwarnings",
    "timeout",
    "tryfirst",
    "trylast",
    "anyio",
    "asyncio",
    "xdist_group",
}


class TestMarkerValidity:
    """Verify that only known markers are used."""

    def test_no_unknown_markers_on_collected_items(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        """Every marker on collected items must be known (project or plugin)."""
        all_known = _KNOWN_MARKERS | _PLUGIN_MARKERS
        unknown: dict[str, set[str]] = {}
        for item in request.session.items:
            for marker in item.iter_markers():
                if marker.name not in all_known:
                    unknown.setdefault(marker.name, set()).add(item.nodeid)
        assert not unknown, (
            f"Unknown markers found: {', '.join(sorted(unknown))}"
        )

    def test_known_markers_defined_in_pyproject(
        self,
        pytestconfig: pytest.Config,
    ) -> None:
        """Every marker in _KNOWN_MARKERS must be defined in pyproject.toml."""
        ini_markers = set(pytestconfig.getini("markers"))
        # getini returns strings like "smoke: core path smoke tests..."
        ini_names = set()
        for m in ini_markers:
            name = m.split(":")[0].split("(")[0].strip()
            ini_names.add(name)
        missing = _KNOWN_MARKERS - ini_names
        assert not missing, (
            f"Markers in _KNOWN_MARKERS but not in pyproject.toml: {sorted(missing)}"
        )


# ---------------------------------------------------------------------------
# Test count soft check
# ---------------------------------------------------------------------------

class TestMarkerCounts:
    """Sanity-check marker count diagnostics without enforcing drifting totals."""

    def test_marker_count_config_is_well_formed(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        """Configured marker counters should name known markers and stay non-negative."""
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")

        expected, tolerance = _configured_marker_counts()
        unknown = set(expected) - _KNOWN_MARKERS
        negative = {marker: count for marker, count in expected.items() if count < 0}

        assert not unknown, f"Unknown marker count config entries: {sorted(unknown)}"
        assert not negative, f"Negative marker count config entries: {negative}"
        assert 0.0 <= tolerance <= 1.0
