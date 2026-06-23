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
    "desktop-architecture-strategy.md",
    "runtime-host-transition.md",
    "naming.md",
]

REQUIRED_ADRS = [
    "0001-local-first-agent.md",
    "0002-cli-is-client-not-core.md",
    "0003-rust-core-tauri-desktop.md",
    "0004-python-as-plugin-runtime.md",
    "0005-mcp-through-tool-broker.md",
    "0006-singularity-project-identity.md",
    "0007-adopt-rust-core-tauri-desktop-strategy.md",
    "0008-runtimehost-as-product-core-boundary.md",
    "0009-python-as-plugin-runtime.md",
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


def test_readme_uses_singularity_identity() -> None:
    text = (ROOT / "README.md").read_text(encoding="utf-8")

    assert text.startswith("# Singularity")
    assert "Singularity" in text


def test_readme_runtime_names_match_runtime_map() -> None:
    readme_names = _runtime_names(ROOT / "README.md")
    runtime_map_names = _runtime_names(ROOT / "docs" / "architecture" / "runtime-map.md")

    assert readme_names == runtime_map_names
    assert "ContextManager" in readme_names
    assert "ContextRuntime" not in readme_names
    assert "DocumentationRuntime" in readme_names
    assert "ParallelToolExecutor" in readme_names
    assert "GitRuntime" in readme_names
    assert "MemorySyncRuntime" in readme_names
    assert "RemoteApprovalRuntime" in readme_names


def test_readme_runtime_status_table_has_source_or_planned_mapping() -> None:
    text = (ROOT / "README.md").read_text(encoding="utf-8")

    assert "## Runtime Capability Status" in text
    assert "| Capability | Status | Source or boundary |" in text
    for status in ("implemented", "partial", "planned"):
        assert f"| {status} |" in text or f" {status} " in text
    assert "`ContextManager` | implemented | `src/singularity/context/manager.py`" in text
    assert "`ContextRuntime` enum | implemented | `src/singularity/context/models.py`" in text
    assert "`FinalReport` | implemented | kernel: `src/singularity/kernel/finalization.py`" in text


def test_readme_implemented_runtime_source_paths_exist() -> None:
    text = (ROOT / "README.md").read_text(encoding="utf-8")

    for capability, status, source in _runtime_status_rows(text):
        if status != "implemented":
            continue
        for relative_path in re.findall(r"`(src/singularity/[^`]+)`", source):
            path = ROOT / relative_path
            assert path.exists(), f"{capability} references missing source path: {relative_path}"


def test_git_runtime_docs_match_local_only_contract() -> None:
    combined = "\n".join(
        path.read_text(encoding="utf-8")
        for path in [
            ROOT / "README.md",
            ROOT / "docs" / "architecture" / "runtime-map.md",
            ROOT / "docs" / "architecture" / "command-runtime.md",
            ROOT / "docs" / "architecture" / "code-index-runtime.md",
            ROOT / "docs" / "architecture" / "verification-runtime.md",
        ]
    )

    assert "Git-absent runtime boundary" not in combined
    assert "GitRuntime` is still reserved" not in combined
    assert "local-only status, diff, and commit" in combined
    assert "Push, pull, reset, remote branches, pull requests" in combined


def test_config_and_sandbox_docs_match_implemented_evidence() -> None:
    readme = (ROOT / "README.md").read_text(encoding="utf-8")
    runtime_map = (ROOT / "docs" / "architecture" / "runtime-map.md").read_text(encoding="utf-8")
    sandbox = (ROOT / "docs" / "architecture" / "sandbox-isolation-runtime.md").read_text(
        encoding="utf-8"
    )

    assert ".singularity/config.toml" in readme
    assert "effective config" in readme
    assert "config source" in readme
    assert "explicit CLI flag > SINGULARITY_* > .singularity/config.toml > defaults" in readme
    assert "DockerSandboxBackend" in sandbox
    assert "LocalStagingBackend" in sandbox
    assert "hard_isolation" in runtime_map
    assert "soft_workspace_isolation" in runtime_map
    assert "no_isolation" in runtime_map
    assert "fails closed" in readme


def _runtime_names(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        r"<!-- runtime-names:start -->(.*?)<!-- runtime-names:end -->",
        text,
        re.DOTALL,
    )
    assert match, f"missing runtime-names markers in {path}"
    return re.findall(r"`([^`]+)`", match.group(1))


def _runtime_status_rows(text: str) -> list[tuple[str, str, str]]:
    rows: list[tuple[str, str, str]] = []
    for line in text.splitlines():
        if not line.startswith("| `"):
            continue
        parts = [part.strip() for part in line.strip("|").split("|")]
        if len(parts) != 3:
            continue
        rows.append((parts[0], parts[1], parts[2]))
    return rows
