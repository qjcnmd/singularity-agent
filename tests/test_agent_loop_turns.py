from __future__ import annotations

import inspect

from singularity.agent_loop_turns import (
    TurnCoordinator,
    TurnCoordinatorCallbacks,
    TurnRuntimeDependencies,
)


def test_turn_coordinator_uses_callback_bundle() -> None:
    signature = inspect.signature(TurnCoordinator.__init__)
    parameters = [
        name
        for name, parameter in signature.parameters.items()
        if name != "self" and parameter.kind is not inspect.Parameter.VAR_KEYWORD
    ]

    assert "callbacks" in parameters
    assert len(parameters) < 15
    assert hasattr(TurnCoordinatorCallbacks, "__dataclass_fields__")
    assert hasattr(TurnRuntimeDependencies, "__dataclass_fields__")
    assert "dependencies" in parameters
    assert not {"model_runner", "tools", "tool_executor", "tool_protocol"}.intersection(parameters)
