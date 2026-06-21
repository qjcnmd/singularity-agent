from __future__ import annotations

import json
import re
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]

REQUIRED_ARCHITECTURE_DOCS = [
    "runtime-map.md",
    "boundary-contracts.md",
    "state-model.md",
    "event-model.md",
    "tool-protocol.md",
    "policy-approval.md",
    "trace-audit.md",
    "migration-to-desktop.md",
]

REQUIRED_ADRS = [
    "0001-local-first-agent.md",
    "0002-cli-is-client-not-core.md",
    "0003-rust-core-tauri-desktop.md",
    "0004-python-as-plugin-runtime.md",
    "0005-mcp-through-tool-broker.md",
]

REQUIRED_SCHEMAS = [
    "run-event.schema.json",
    "session-state.schema.json",
    "tool-call.schema.json",
    "approval.schema.json",
    "trace-span.schema.json",
    "artifact.schema.json",
    "memory-item.schema.json",
]


def test_documentation_runtime_architecture_docs_exist() -> None:
    for name in REQUIRED_ARCHITECTURE_DOCS:
        path = ROOT / "docs" / "architecture" / name
        assert path.exists(), f"missing docs/architecture/{name}"
        assert path.read_text(encoding="utf-8").lstrip().startswith("# ")


def test_documentation_runtime_adr_files_exist() -> None:
    for name in REQUIRED_ADRS:
        path = ROOT / "docs" / "adr" / name
        assert path.exists(), f"missing docs/adr/{name}"
        text = path.read_text(encoding="utf-8")
        assert text.startswith("# ADR ")
        assert "Status:" in text


def test_documentation_runtime_schema_files_exist_and_parse() -> None:
    for name in REQUIRED_SCHEMAS:
        path = ROOT / "docs" / "schemas" / name
        assert path.exists(), f"missing docs/schemas/{name}"
        schema = json.loads(path.read_text(encoding="utf-8"))
        assert schema["$schema"].startswith("https://json-schema.org/")
        assert schema["type"] == "object"
        assert schema["title"]


def test_readme_runtime_names_match_runtime_map() -> None:
    readme_names = _runtime_names(ROOT / "README.md")
    runtime_map_names = _runtime_names(ROOT / "docs" / "architecture" / "runtime-map.md")

    assert readme_names == runtime_map_names
    assert "DocumentationRuntime" in readme_names
    assert "GitRuntime" not in readme_names


def _runtime_names(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        r"<!-- runtime-names:start -->(.*?)<!-- runtime-names:end -->",
        text,
        re.DOTALL,
    )
    assert match, f"missing runtime-names markers in {path}"
    return re.findall(r"`([^`]+)`", match.group(1))
