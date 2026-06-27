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

from pathlib import Path

import pytest

from tests.conftest import (
    _EXTERNAL_FILE_STEMS,
    _FLAKY_TEST_IDS,
    _KNOWN_MARKERS,
    _SMOKE_FILE_STEMS,
    _SMOKE_TEST_IDS,
    _SLOW_TEST_IDS,
)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_FULL_SUITE_THRESHOLD = 100  # minimum items to consider collection complete


def _is_full_collection(request: pytest.FixtureRequest) -> bool:
    """Check if the current collection represents the full test suite."""
    return len(request.session.items) >= _FULL_SUITE_THRESHOLD


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
    for marker in markers:
        if f"not {marker}" in combined:
            return True
    return False


# ---------------------------------------------------------------------------
# Curated list existence checks
# ---------------------------------------------------------------------------

class TestCuratedListIntegrity:
    """Verify that every test ID in curated lists actually exists.

    These tests only run when the full suite is collected (>=100 items).
    Lists filtered by default addopts (slow, external) are skipped when
    those markers are excluded.
    """

    def test_smoke_test_ids_all_exist(self, request: pytest.FixtureRequest) -> None:
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")
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
    """Soft check that marker counts are within expected ranges.

    These are approximate — the exact counts change as tests are added or
    reclassified.  A ±30% tolerance avoids brittle failures while still
    catching major misconfigurations.
    """

    # Expected counts (approximate, used for soft validation).
    # Lists filtered by default addopts have expected count 0.
    _EXPECTED = {
        "smoke": 26,
        "unit": 550,
        "integration": 175,
        "regression": 68,
        "security": 54,
        "flaky": 4,
        "evaluation": 0,   # excluded by default addopts
        "slow": 0,         # excluded by default addopts
        "external": 0,     # excluded by default addopts
    }

    def test_marker_counts_reasonable(
        self,
        request: pytest.FixtureRequest,
    ) -> None:
        """Each marker count should be within ±30% of expected."""
        if not _is_full_collection(request):
            pytest.skip("Requires full test collection")

        from collections import Counter

        counts: Counter[str] = Counter()
        for item in request.session.items:
            for marker in item.iter_markers():
                if marker.name in self._EXPECTED:
                    counts[marker.name] += 1

        for marker, expected in self._EXPECTED.items():
            if expected == 0:
                continue  # excluded by addopts, skip
            actual = counts.get(marker, 0)
            low = int(expected * 0.7)
            high = int(expected * 1.3)
            if actual < low or actual > high:
                import warnings
                warnings.warn(
                    f"Marker '{marker}' count {actual} outside expected "
                    f"range [{low}, {high}] (expected ~{expected})"
                )
