from __future__ import annotations

from types import SimpleNamespace

from singularity.utils.attributes import nested_getattr


def test_nested_getattr_returns_nested_value() -> None:
    value = SimpleNamespace(
        graph=SimpleNamespace(trace=SimpleNamespace(store=SimpleNamespace(run_dir="trace-dir")))
    )

    assert nested_getattr(value, "graph.trace.store.run_dir") == "trace-dir"


def test_nested_getattr_returns_default_for_missing_link() -> None:
    value = SimpleNamespace(graph=None)

    assert nested_getattr(value, "graph.trace.store.run_dir", default="") == ""
