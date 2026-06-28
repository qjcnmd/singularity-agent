from __future__ import annotations

import json
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

MODULE_DOCS = {
    "agent-loop.md",
    "artifact-long-result-handling.md",
    "command-execution.md",
    "context-assembly-prompt-frame.md",
    "context-compaction-observation-store.md",
    "evaluation-benchmark-runner.md",
    "failure-analysis-repair.md",
    "kernel-agent-graph.md",
    "memory-index-context.md",
    "model-turn-provider-tools.md",
    "planner-replanner-failure-recovery.md",
    "plugin-tools-registry.md",
    "policy-approval-gates.md",
    "sandbox-isolation.md",
    "tool-execution.md",
    "tool-registry-exposure.md",
    "trace-observation-audit-events.md",
    "verification-contract-satisfaction.md",
}

REQUIRED_SCHEMAS = [
    "run-event.schema.json",
    "session-state.schema.json",
    "tool-call.schema.json",
    "approval.schema.json",
    "trace-span.schema.json",
    "artifact.schema.json",
    "memory-item.schema.json",
]

RETIRED_DOC_PATHS = [
    "docs/" + "adr",
    "docs/evaluation" + "-harness.md",
    "docs/architecture/execution" + "-map.md",
    "docs/architecture/naming" + "-and-concept-map.md",
    "docs/architecture/migration" + "-to-desktop.md",
    "docs/architecture/desktop" + "-architecture-strategy.md",
    "docs/architecture/agent" + "-host-transition.md",
]


def test_only_module_architecture_docs_remain() -> None:
    architecture_root = ROOT / "docs" / "architecture"
    files = {
        path.relative_to(architecture_root).as_posix()
        for path in architecture_root.rglob("*.md")
    }

    assert files == {f"modules/{name}" for name in MODULE_DOCS}


def test_module_docs_are_chinese_data_flow_docs() -> None:
    for name in MODULE_DOCS:
        path = ROOT / "docs" / "architecture" / "modules" / name
        text = path.read_text(encoding="utf-8")

        assert text.startswith("# ")
        assert "模块数据流文档 ID:" in text
        assert "源码证据路径:" in text
        assert "字段清单:" in text
        assert "## 真实运行时调用链" in text
        assert _cjk_count(text) > 100


def test_retired_doc_paths_do_not_exist() -> None:
    for relative in RETIRED_DOC_PATHS:
        assert not (ROOT / relative).exists(), f"retired doc path still exists: {relative}"


def test_readme_points_to_current_module_docs() -> None:
    text = (ROOT / "README.md").read_text(encoding="utf-8")

    assert text.startswith("# Singularity")
    assert "docs/architecture/modules/" in text
    assert "docs/architecture/execution" + "-map.md" not in text
    assert "docs/evaluation" + "-harness.md" not in text
    assert "docs/" + "adr" not in text
    assert "旧阶段报告" in text
    assert _cjk_count(text) > 200


def test_schema_files_exist_and_parse() -> None:
    for name in REQUIRED_SCHEMAS:
        path = ROOT / "docs" / "schemas" / name
        assert path.exists(), f"missing docs/schemas/{name}"
        schema = json.loads(path.read_text(encoding="utf-8"))
        assert schema["$schema"].startswith("https://json-schema.org/")
        assert schema["type"] == "object"
        assert schema["title"]


def test_docs_do_not_reintroduce_retired_terms() -> None:
    combined = "\n".join(
        path.read_text(encoding="utf-8")
        for path in (ROOT / "docs").rglob("*.md")
    )

    forbidden = [
        "Runtime" + " Flow",
        "deprecated compatibility" + " alias",
        "migration" + " input",
        "retired" + " live",
        "eval" + " live",
        "Live" + "Eval",
        "Live" + "Agent",
    ]
    for term in forbidden:
        assert term not in combined


def _cjk_count(text: str) -> int:
    return len(re.findall(r"[\u4e00-\u9fff]", text))
