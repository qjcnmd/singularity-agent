from __future__ import annotations

import re

from singularity.instructions.models import (
    InjectionWarning,
    InstructionSource,
    InstructionSourceType,
    _new_id,
)


DETECT_SOURCE_TYPES = {
    InstructionSourceType.PROJECT_FILE,
    InstructionSourceType.README,
    InstructionSourceType.TOOL_OUTPUT,
    InstructionSourceType.COMMAND_OUTPUT,
    InstructionSourceType.MODEL_OUTPUT,
    InstructionSourceType.CONTEXT_SUMMARY,
    InstructionSourceType.PROJECT_INSTRUCTION_FILE,
}


PATTERNS: list[tuple[str, re.Pattern[str], str]] = [
    ("ignore_previous_instructions", re.compile(r"ignore\s+(all\s+)?previous\s+instructions?", re.I), "warning"),
    ("disregard_higher_instructions", re.compile(r"disregard\s+(system|developer|user)\s+instructions?", re.I), "warning"),
    ("reveal_hidden_prompt", re.compile(r"reveal\s+(hidden|system)\s+prompt", re.I), "critical"),
    ("bypass_policy_approval_sandbox", re.compile(r"bypass\s+(policy|approval|sandbox)|policy\s*/\s*approval\s*/\s*sandbox", re.I), "critical"),
    ("run_command_immediately", re.compile(r"run\s+this\s+command\s+immediately", re.I), "warning"),
    ("delete_files", re.compile(r"delete\s+(all\s+)?files?", re.I), "critical"),
    ("exfiltrate_secrets", re.compile(r"exfiltrate\s+secrets?", re.I), "critical"),
    ("read_env_private_key", re.compile(r"read\s+(`?\.env`?|private\s+key)|private\s+key", re.I), "critical"),
    ("override_tool_rules", re.compile(r"override\s+tool\s+rules?", re.I), "critical"),
    ("pretend_user_approved", re.compile(r"pretend\s+user\s+approved", re.I), "critical"),
    ("chinese_ignore_previous", re.compile(r"忽略之前的指令"), "warning"),
    ("chinese_ignore_system", re.compile(r"忽略系统指令"), "warning"),
    ("chinese_bypass_safety", re.compile(r"不要遵守安全规则"), "critical"),
    ("chinese_user_approved", re.compile(r"用户已经批准"), "critical"),
    ("chinese_read_secret", re.compile(r"读取密钥"), "critical"),
    ("chinese_delete_all", re.compile(r"删除所有文件"), "critical"),
]


class PromptInjectionDetector:
    def detect(self, source: InstructionSource) -> list[InjectionWarning]:
        warnings: list[InjectionWarning] = []
        for name, pattern, severity in PATTERNS:
            match = pattern.search(source.content)
            if not match:
                continue
            warnings.append(
                InjectionWarning(
                    warning_id=_new_id("injection"),
                    source_id=source.source_id,
                    pattern=name,
                    message=f"Potential prompt injection detected: {name}.",
                    severity=severity,
                    evidence_excerpt=_excerpt(source.content, match.start(), match.end()),
                    metadata={
                        "source_type": source.source_type.value,
                        "origin": source.origin,
                    },
                )
            )
        return warnings

    def detect_many(self, sources: list[InstructionSource]) -> list[InjectionWarning]:
        warnings: list[InjectionWarning] = []
        for source in sources:
            warnings.extend(self.detect(source))
        return warnings


def _excerpt(text: str, start: int, end: int, *, radius: int = 48) -> str:
    return text[max(0, start - radius) : min(len(text), end + radius)]
