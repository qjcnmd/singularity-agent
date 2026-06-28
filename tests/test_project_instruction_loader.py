from pathlib import Path

import pytest

from singularity.instructions import (
    InstructionSourceType,
    ProjectInstructionLoader,
    PromptAssemblyConfig,
    TrustLevel,
)
from singularity.instructions.exceptions import InstructionSourceError


def test_loads_agents_and_singularity_instruction_files(tmp_path: Path) -> None:
    (tmp_path / "AGENTS.md").write_text("Project rules", encoding="utf-8")
    (tmp_path / ".singularity").mkdir()
    (tmp_path / ".singularity" / "instructions.md").write_text("Singularity rules", encoding="utf-8")
    loader = ProjectInstructionLoader(tmp_path)

    sources = loader.load()

    assert [source.origin for source in sources] == [
        str((tmp_path / "AGENTS.md").resolve()),
        str((tmp_path / ".singularity" / "instructions.md").resolve()),
    ]
    assert all(source.trust_level == TrustLevel.PROJECT_DECLARED for source in sources)
    assert all(source.source_type == InstructionSourceType.PROJECT_INSTRUCTION_FILE for source in sources)


def test_loader_rejects_workspace_escape(tmp_path: Path) -> None:
    loader = ProjectInstructionLoader(
        tmp_path,
        config=PromptAssemblyConfig(project_instruction_filenames=["../outside.md"]),
    )

    with pytest.raises(InstructionSourceError):
        loader.load()


def test_loader_truncates_large_files_and_does_not_require_git(tmp_path: Path) -> None:
    (tmp_path / "AGENTS.md").write_text("x" * 128, encoding="utf-8")
    loader = ProjectInstructionLoader(
        tmp_path,
        config=PromptAssemblyConfig(max_project_instruction_bytes=16),
    )

    sources = loader.load()

    assert len(sources[0].content.encode("utf-8")) <= 16
    assert sources[0].metadata["truncated"] is True
    assert sources[0].source_hash
