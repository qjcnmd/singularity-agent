from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def _load_verify_module():
    path = ROOT / "scripts" / "verify_runtime_docs.py"
    spec = importlib.util.spec_from_file_location("verify_runtime_docs", path)
    assert spec is not None
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_extract_field_checks_parses_explicit_doc_block() -> None:
    module = _load_verify_module()

    text = """
Field checks:
- ToolResult: ok, content, error_code
- ModelTurnRequest: request_id, messages, tools

## Module Boundary
"""

    assert module._extract_field_checks(text) == {
        "ToolResult": ["ok", "content", "error_code"],
        "ModelTurnRequest": ["request_id", "messages", "tools"],
    }


def test_class_fields_in_sources_reads_annotated_class_fields(tmp_path: Path) -> None:
    module = _load_verify_module()
    source = tmp_path / "models.py"
    source.write_text(
        """
from dataclasses import dataclass, field


@dataclass
class RuntimeObject:
    request_id: str
    metadata: dict[str, str] = field(default_factory=dict)
    save = method

    def method(self) -> None:
        pass
""".lstrip(),
        encoding="utf-8",
    )

    assert module._class_fields_in_sources([source]) == {
        "RuntimeObject": {"request_id", "metadata"}
    }
    assert "RuntimeObject.save" in module._symbols_in_sources([source])
