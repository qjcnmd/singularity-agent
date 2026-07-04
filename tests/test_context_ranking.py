import inspect

from singularity.context import assembler, compaction
from singularity.context.models import ContextAuthority, ContextLayer
from singularity.context.ranking import (
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
