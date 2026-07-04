from __future__ import annotations

from typing import Any

__all__ = [
    "AgentGraph",
    "AgentGraphBuilder",
    "AgentKernel",
    "AgentRun",
    "AgentSession",
    "CancellationError",
    "CancellationManager",
    "CancellationReason",
    "CancellationToken",
    "ComponentHealthChecker",
    "ComponentHealthReport",
    "ComponentName",
    "ComponentState",
    "CrashRecoveryManager",
    "FinalReport",
    "KernelBootstrap",
    "KernelContext",
    "KernelError",
    "KernelFinalizer",
    "KernelStatus",
    "LifecycleEvent",
    "PartialFinalReport",
    "RecoveryReport",
    "RunIdentity",
    "RunLifecycleManager",
    "RunResult",
    "RunStatus",
    "SessionStatus",
    "ShutdownManager",
    "ShutdownReason",
    "ShutdownSummary",
    "WorkspaceLockManager",
]

_EXPORT_MODULES = {
    "AgentKernel": "singularity.kernel.agent_kernel",
    "RunResult": "singularity.kernel.agent_kernel",
    "KernelBootstrap": "singularity.kernel.bootstrap",
    "CancellationManager": "singularity.kernel.cancellation",
    "CancellationToken": "singularity.kernel.cancellation",
    "CancellationError": "singularity.kernel.exceptions",
    "KernelError": "singularity.kernel.exceptions",
    "FinalReport": "singularity.kernel.finalization",
    "KernelFinalizer": "singularity.kernel.finalization",
    "PartialFinalReport": "singularity.kernel.finalization",
    "AgentGraph": "singularity.kernel.graph",
    "AgentGraphBuilder": "singularity.kernel.graph",
    "ComponentHealthChecker": "singularity.kernel.health",
    "ComponentHealthReport": "singularity.kernel.health",
    "RunLifecycleManager": "singularity.kernel.lifecycle",
    "WorkspaceLockManager": "singularity.kernel.locks",
    "AgentRun": "singularity.kernel.models",
    "AgentSession": "singularity.kernel.models",
    "CancellationReason": "singularity.kernel.models",
    "ComponentName": "singularity.kernel.models",
    "ComponentState": "singularity.kernel.models",
    "KernelContext": "singularity.kernel.models",
    "KernelStatus": "singularity.kernel.models",
    "LifecycleEvent": "singularity.kernel.models",
    "RunIdentity": "singularity.kernel.models",
    "RunStatus": "singularity.kernel.models",
    "SessionStatus": "singularity.kernel.models",
    "ShutdownReason": "singularity.kernel.models",
    "CrashRecoveryManager": "singularity.kernel.recovery",
    "RecoveryReport": "singularity.kernel.recovery",
    "ShutdownManager": "singularity.kernel.shutdown",
    "ShutdownSummary": "singularity.kernel.shutdown",
}


def __getattr__(name: str) -> Any:
    module_name = _EXPORT_MODULES.get(name)
    if module_name is None:
        raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
    module = __import__(module_name, fromlist=[name])
    value = getattr(module, name)
    globals()[name] = value
    return value
