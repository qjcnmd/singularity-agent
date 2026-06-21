from singularity.instructions import (
    InjectionWarning,
    InstructionConflict,
    InstructionFrame,
    InstructionPriority,
    InstructionScope,
    InstructionSource,
    InstructionSourceType,
    PromptManifestBuilder,
    PromptSection,
    TrustLevel,
)


def test_manifest_counts_sources_sections_conflicts_and_warnings_without_full_prompt() -> None:
    source = InstructionSource(
        source_id="source_1",
        source_type=InstructionSourceType.USER_MESSAGE,
        origin="chat",
        priority=InstructionPriority.USER_TASK,
        trust_level=TrustLevel.TRUSTED_USER,
        scope=InstructionScope(),
        content="OPENAI_API_KEY=sk-secret should not appear",
    )
    frame = InstructionFrame(
        frame_id="frame_1",
        source=source,
        normalized_content=source.content,
        effective_priority=source.priority,
        effective_trust_level=source.trust_level,
    )
    section = PromptSection(
        section_id="section_1",
        title="User",
        priority=source.priority,
        trust_level=source.trust_level,
        source_refs=[source.source_id],
        content=source.content,
        token_estimate=10,
    )
    conflict = InstructionConflict(
        conflict_id="conflict_1",
        higher_source_id="user",
        lower_source_id="project",
        description="Project conflicts with user.",
        resolution="User wins.",
        severity="warning",
    )
    warning = InjectionWarning(
        warning_id="warning_1",
        source_id=source.source_id,
        pattern="read_env_private_key",
        message="secret request",
        severity="critical",
        evidence_excerpt=".env",
    )

    manifest = PromptManifestBuilder().build(
        bundle_id="bundle_1",
        purpose="plan_next_action",
        frames=[frame],
        sections=[section],
        conflicts=[conflict],
        warnings=[warning],
        prompt_hash="hash",
        token_estimate=10,
    )

    payload = manifest.to_dict()
    assert payload["source_count"] == 1
    assert payload["section_count"] == 1
    assert payload["conflict_count"] == 1
    assert payload["injection_warning_count"] == 1
    assert payload["redaction_applied"] is True
    assert "sk-secret" not in str(payload)
    assert "OPENAI_API_KEY" not in str(payload)
