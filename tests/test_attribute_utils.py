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


def test_nested_getattr_reads_enum_value_style_attribute() -> None:
    value = SimpleNamespace(status=SimpleNamespace(value="ready"))

    assert nested_getattr(value, "status.value", default="") == "ready"


def test_production_code_has_no_nested_getattr_chain() -> None:
    from pathlib import Path

    offenders = [
        str(path)
        for path in Path("src/singularity").rglob("*.py")
        if "getattr(getattr(" in path.read_text(encoding="utf-8")
    ]

    assert offenders == []
