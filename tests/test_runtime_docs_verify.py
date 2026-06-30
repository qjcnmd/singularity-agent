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


def test_extract_field_checks_parses_chinese_doc_block() -> None:
    module = _load_verify_module()

    text = """
字段清单:
- ToolResult: ok, content, error_code
- ModelTurnRequest: request_id, messages, tools

## 维护规则
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


def test_forbidden_keyword_list_avoids_literal_old_terms() -> None:
    module = _load_verify_module()

    forbidden = module._forbidden_terms()

    assert "LEGACY" + "_LIVE" in forbidden
    assert "evaluation." + "live" + "_agent" in forbidden
    assert "Runtime" + " Flow" in forbidden


def test_ensure_utf8_stdio_reconfigures_output_streams(monkeypatch) -> None:
    module = _load_verify_module()

    class RecordingStream:
        def __init__(self) -> None:
            self.calls = []

        def reconfigure(self, **kwargs) -> None:
            self.calls.append(kwargs)

    stdout = RecordingStream()
    stderr = RecordingStream()
    monkeypatch.setattr(module.sys, "stdout", stdout)
    monkeypatch.setattr(module.sys, "stderr", stderr)

    module._ensure_utf8_stdio()

    expected = [{"encoding": "utf-8", "errors": "replace"}]
    assert stdout.calls == expected
    assert stderr.calls == expected


def test_verify_doc_rejects_generic_data_flow_template(tmp_path: Path) -> None:
    module = _load_verify_module()
    path = tmp_path / "module.md"
    phrase = "这些对象由上文列出的源码组件在运行链路中生成"
    doc = module.ModuleDataFlowDoc(
        path=path,
        text=phrase,
        doc_id="test-module",
        source_paths=[],
        symbols=[],
        field_checks={},
        headings=set(),
    )
    errors: list[str] = []

    module._verify_doc(doc, errors)

    assert any(f"forbidden template phrase remains: {phrase}" in error for error in errors)
