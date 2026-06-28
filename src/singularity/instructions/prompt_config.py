from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class PromptAssemblyConfig:
    enable_project_instructions: bool = True
    project_instruction_filenames: list[str] = field(
        default_factory=lambda: [
            "AGENTS.md",
            ".singularity/instructions.md",
            ".singularity/AGENTS.md",
        ]
    )
    max_project_instruction_bytes: int = 64 * 1024
    max_untrusted_content_bytes: int = 16 * 1024
    max_prompt_tokens: int | None = None
    include_instruction_hierarchy_notice: bool = True
    include_prompt_injection_notice: bool = True
    redact_before_compile: bool = True
    store_prompt_manifest: bool = True
    store_full_prompt_artifact: bool = False
    fail_on_critical_injection: bool = False
