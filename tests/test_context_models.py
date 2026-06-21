from singularity.context.models import (
    ContextAuthority,
    ContextFreshness,
    ContextItem,
    ContextItemType,
    ContextLayer,
    ContextReference,
    ContextRenderPolicy,
    ContextRuntime,
    ContextSensitivity,
)


def test_context_item_carries_production_ledger_fields() -> None:
    reference = ContextReference(
        ref_id="ref_readme",
        ref_type="file",
        target="README.md",
        path="README.md",
        line_start=1,
        line_end=10,
        digest="abc123",
        source_item_id="item_readme",
    )
    item = ContextItem(
        item_id="item_readme",
        run_id="run_1",
        session_id="session_1",
        task_id="task_1",
        phase_id="inspect",
        layer=ContextLayer.EVIDENCE,
        source_runtime=ContextRuntime.TOOL,
        item_type=ContextItemType.TOOL_OBSERVATION,
        content="readme content",
        authority=ContextAuthority.TOOL,
        sensitivity=ContextSensitivity.WORKSPACE,
        references=[reference],
        token_count=5,
        importance=0.8,
    )

    assert item.content_digest
    assert item.freshness == ContextFreshness.CURRENT
    assert item.references[0].ref_id == "ref_readme"
    assert item.references[0].id == "ref_readme"
    assert item.references[0].type == "file"
    assert item.references[0].observation_id == "item_readme"


def test_render_policy_defaults_are_safe_for_model_export() -> None:
    policy = ContextRenderPolicy()

    assert policy.include_raw_tool_outputs is False
    assert policy.include_secret_content is False
    assert policy.redact_sensitive is True
    assert policy.require_references_for_claims is True

