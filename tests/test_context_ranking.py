import inspect

from singularity.context import assembler, compaction
from singularity.context.models import ContextAuthority, ContextLayer
from singularity.context.ranking import (
    CONTEXT_COMPACTION_PINNED_SCORE_BONUS,
    CONTEXT_COMPACTION_STALE_FRESHNESS_SCORE_PENALTY,
    CONTEXT_COMPACTION_WEIGHT_SCALE,
    CONTEXT_CURRENT_FRESHNESS_SCORE_BONUS,
    CONTEXT_PHASE_MATCH_SCORE_BONUS,
    CONTEXT_PINNED_ITEM_SCORE_BONUS,
    CONTEXT_RECENCY_CURRENT,
    CONTEXT_RECENCY_DEFAULT,
    CONTEXT_RECENCY_STALE,
    CONTEXT_REFERENCE_DENSITY_PRECISION,
    CONTEXT_STALE_FRESHNESS_SCORE_PENALTY,
    CONTEXT_VOLATILITY_DEFAULT,
    CONTEXT_VOLATILITY_EVIDENCE,
    CONTEXT_VOLATILITY_MESSAGE_OR_FAILURE,
    CONTEXT_VOLATILITY_RECENT_OR_FAILURE,
    authority_weight,
    layer_order,
    layer_weight,
)


def test_context_ranking_weights_are_shared_by_assembler_and_compaction() -> None:
    assert layer_weight(ContextLayer.SYSTEM) == 100.0
    assert layer_weight(ContextLayer.TOOL_OBSERVATIONS) == 24.0
    assert authority_weight(ContextAuthority.COMPONENT) == 7.0
    assert authority_weight(ContextAuthority.MODEL) == 1.0
    assert layer_order(ContextLayer.SYSTEM) == 0
    assert layer_order(ContextLayer.PLANNER_STATE) == 4

    assembler_source = inspect.getsource(assembler)
    compaction_source = inspect.getsource(compaction.ContextCompactionPlanner)

    assert "def _layer_weight" not in assembler_source
    assert "def _authority_weight" not in assembler_source
    assert "def _layer_order" not in assembler_source
    assert "def layer_weight" not in compaction_source
    assert "def authority_weight" not in compaction_source


def test_context_scoring_policy_values_are_named_constants() -> None:
    assert CONTEXT_PINNED_ITEM_SCORE_BONUS == 100.0
    assert CONTEXT_PHASE_MATCH_SCORE_BONUS == 10.0
    assert CONTEXT_CURRENT_FRESHNESS_SCORE_BONUS == 2.0
    assert CONTEXT_STALE_FRESHNESS_SCORE_PENALTY == -2.0
    assert CONTEXT_COMPACTION_WEIGHT_SCALE == 100.0
    assert CONTEXT_COMPACTION_PINNED_SCORE_BONUS == 10.0
    assert CONTEXT_COMPACTION_STALE_FRESHNESS_SCORE_PENALTY == -1.0
    assert CONTEXT_VOLATILITY_RECENT_OR_FAILURE == 1.0
    assert CONTEXT_VOLATILITY_MESSAGE_OR_FAILURE == 0.8
    assert CONTEXT_VOLATILITY_EVIDENCE == 0.5
    assert CONTEXT_VOLATILITY_DEFAULT == 0.2
    assert CONTEXT_RECENCY_CURRENT == 1.0
    assert CONTEXT_RECENCY_STALE == 0.3
    assert CONTEXT_RECENCY_DEFAULT == 0.0
    assert CONTEXT_REFERENCE_DENSITY_PRECISION == 4


def test_context_scoring_call_sites_do_not_embed_policy_values() -> None:
    score_item_source = inspect.getsource(assembler.ContextAssembler._score_item)
    compaction_source = inspect.getsource(compaction.ContextCompactionPlanner)

    for raw_policy_value in (
        "score += 100",
        "score += 10",
        "score += 2",
        "score -= 2",
    ):
        assert raw_policy_value not in score_item_source

    for raw_policy_value in (
        "score += 10.0",
        "score -= 1.0",
        "round(refs / tokens, 4)",
    ):
        assert raw_policy_value not in compaction_source
