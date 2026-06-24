from singularity.instructions import (
    InstructionFrame,
    InstructionPriority,
    InstructionScope,
    InstructionSource,
    InstructionSourceType,
    PromptBundle,
    PromptManifest,
    PromptSection,
    TrustLevel,
)
from singularity.model import ModelMessage, ModelRole


def test_instruction_models_construct_and_serialize() -> None:
    scope = InstructionScope(
        applies_to_component=["model"],
        applies_to_purpose=["plan_next_action"],
        applies_to_paths=["src/**"],
        applies_to_tools=["read_file"],
        session_only=True,
        task_only=False,
    )
    source = InstructionSource(
        source_id="source_1",
        source_type=InstructionSourceType.USER_MESSAGE,
        origin="chat",
        priority=InstructionPriority.USER_TASK,
        trust_level=TrustLevel.TRUSTED_USER,
        scope=scope,
        content="Inspect the component.",
    )
    frame = InstructionFrame(
        frame_id="frame_1",
        source=source,
        normalized_content="Inspect the component.",
        effective_priority=InstructionPriority.USER_TASK,
        effective_trust_level=TrustLevel.TRUSTED_USER,
    )
    section = PromptSection(
        section_id="section_1",
        title="User task",
        priority=InstructionPriority.USER_TASK,
        trust_level=TrustLevel.TRUSTED_USER,
        source_refs=["source_1"],
        content="Inspect the component.",
        token_estimate=4,
    )
    manifest = PromptManifest(
        manifest_id="manifest_1",
        bundle_id="bundle_1",
        purpose="plan_next_action",
        source_count=1,
        section_count=1,
        trust_summary={TrustLevel.TRUSTED_USER.value: 1},
        priority_summary={InstructionPriority.USER_TASK.value: 1},
        prompt_hash="hash",
        token_estimate=4,
    )
    bundle = PromptBundle(
        bundle_id="bundle_1",
        purpose="plan_next_action",
        messages=[ModelMessage(role=ModelRole.USER, content=[])],
        sections=[section],
        manifest=manifest,
        token_estimate=4,
        prompt_hash="hash",
    )

    assert InstructionSource.from_dict(source.to_dict()).source_hash == source.source_hash
    assert InstructionFrame.from_dict(frame.to_dict()).source.source_id == "source_1"
    assert PromptSection.from_dict(section.to_dict()).priority == InstructionPriority.USER_TASK
    assert PromptManifest.from_dict(manifest.to_dict()).trust_summary == {TrustLevel.TRUSTED_USER.value: 1}
    assert PromptBundle.from_dict(bundle.to_dict()).manifest.manifest_id == "manifest_1"


def test_priority_comparison_is_stable() -> None:
    assert InstructionPriority.SYSTEM_INVARIANT > InstructionPriority.SINGULARITY_DEVELOPER
    assert InstructionPriority.USER_TASK > InstructionPriority.PROJECT_INSTRUCTION
    assert InstructionPriority.RETRIEVED_CONTENT > InstructionPriority.MODEL_GENERATED
