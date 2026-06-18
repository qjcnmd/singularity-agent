from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from miniharness.command import CommandRuntime
from miniharness.config import ProductionRuntimeConfig
from miniharness.agent import SYSTEM_PROMPT
from miniharness.context import ContextManager
from miniharness.instructions import InstructionRuntime
from miniharness.model import (
    ModelProviderRegistry,
    ModelRuntime,
    OpenAICompatibleModelProvider,
)
from miniharness.observability import TraceRuntime
from miniharness.policy import ApprovalGate, PolicyRuntime
from miniharness.planner import PlannerRuntime
from miniharness.sandbox import SandboxRuntime
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.tools.command import register_command_tools
from miniharness.tools.mutation import register_mutation_tools
from miniharness.tools.verification import register_verification_tools
from miniharness.tools.workspace_state import register_workspace_state_tools
from miniharness.verification import VerificationRuntime
from miniharness.workspace import MutationRuntime
from miniharness.workspace_state import LocalWorkspaceStateRuntime, WorkspaceHealthReport

from miniharness.kernel.exceptions import RuntimeInitializationError
from miniharness.kernel.models import (
    RuntimeComponentName,
    RuntimeComponentState,
    RunIdentity,
)


RUNTIME_INITIALIZATION_ORDER = [
    RuntimeComponentName.CONFIGURATION,
    RuntimeComponentName.OBSERVABILITY,
    RuntimeComponentName.WORKSPACE_STATE,
    RuntimeComponentName.POLICY,
    RuntimeComponentName.SANDBOX,
    RuntimeComponentName.COMMAND,
    RuntimeComponentName.MUTATION,
    RuntimeComponentName.TOOLS,
    RuntimeComponentName.VERIFICATION,
    RuntimeComponentName.INSTRUCTIONS,
    RuntimeComponentName.MODEL,
    RuntimeComponentName.CONTEXT,
    RuntimeComponentName.PLANNER,
]


@dataclass
class RuntimeGraph:
    config: ProductionRuntimeConfig
    trace: TraceRuntime
    workspace_state: LocalWorkspaceStateRuntime
    policy_runtime: PolicyRuntime
    approval_gate: ApprovalGate
    sandbox_runtime: SandboxRuntime
    command_runtime: CommandRuntime
    mutation_runtime: MutationRuntime
    tools: ToolRegistry
    verification_runtime: VerificationRuntime
    instruction_runtime: InstructionRuntime
    model_runtime: ModelRuntime
    context_manager: ContextManager
    tool_runtime: ToolRuntime
    protocol_runtime: ToolCallingProtocolRuntime
    planner: PlannerRuntime
    initialization_order: list[RuntimeComponentName] = field(
        default_factory=lambda: list(RUNTIME_INITIALIZATION_ORDER)
    )
    components: dict[RuntimeComponentName, RuntimeComponentState] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not self.components:
            self.components = {
                component: RuntimeComponentState.READY
                for component in self.initialization_order
            }

    def state(self, component: RuntimeComponentName) -> RuntimeComponentState:
        return self.components.get(component, RuntimeComponentState.PENDING)

    def components_for_health(self) -> dict[str, Any]:
        return {
            "config": self.config,
            "trace": self.trace,
            "workspace": self.workspace_state,
            "policy": self.policy_runtime,
            "sandbox": self.sandbox_runtime,
            "command": self.command_runtime,
            "mutation": self.mutation_runtime,
            "tools": self.tools,
            "verification": self.verification_runtime,
            "instructions": self.instruction_runtime,
            "model": self.model_runtime,
            "planner": self.planner,
        }


class RuntimeFactory:
    def build(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        identity: RunIdentity,
        user_goal: str,
        workspace_health: WorkspaceHealthReport | None = None,
    ) -> RuntimeGraph:
        components = {
            component: RuntimeComponentState.PENDING
            for component in RUNTIME_INITIALIZATION_ORDER
        }

        def mark(component: RuntimeComponentName) -> None:
            components[component] = RuntimeComponentState.READY
            trace.record(
                "runtime.initialized",
                {"component": component.value, "state": RuntimeComponentState.READY.value},
            )

        try:
            mark(RuntimeComponentName.CONFIGURATION)
            mark(RuntimeComponentName.OBSERVABILITY)

            state_runtime = LocalWorkspaceStateRuntime(project_root, trace=trace)
            if config.resume_session:
                state_runtime.recover_session(config.resume_session)
            else:
                state_runtime.begin_session(task_id=identity.task_id, session_id=identity.session_id)
            mark(RuntimeComponentName.WORKSPACE_STATE)

            policy_config = config.to_policy_config()
            policy_runtime = PolicyRuntime(policy_config, trace=trace)
            approval_gate = ApprovalGate(policy_config, trace=trace)
            mark(RuntimeComponentName.POLICY)

            sandbox_runtime = SandboxRuntime(project_root, trace=trace)
            mark(RuntimeComponentName.SANDBOX)

            command_runtime = CommandRuntime(
                project_root,
                trace=trace,
                state_runtime=state_runtime,
                planner=None,
                policy_runtime=policy_runtime,
                sandbox_runtime=sandbox_runtime,
            )
            mark(RuntimeComponentName.COMMAND)

            mutation_runtime = MutationRuntime(
                project_root,
                trace=trace,
                state_runtime=state_runtime,
                planner=None,
                policy_runtime=policy_runtime,
            )
            mark(RuntimeComponentName.MUTATION)

            tools = ToolRegistry(project_root)
            register_mutation_tools(tools, mutation_runtime)
            register_command_tools(tools, command_runtime)
            register_workspace_state_tools(tools, state_runtime)
            mark(RuntimeComponentName.TOOLS)

            verification_runtime = VerificationRuntime(
                project_root,
                command_runtime=command_runtime,
                trace=trace,
                planner=None,
                policy_runtime=policy_runtime,
            )
            register_verification_tools(tools, verification_runtime)
            mark(RuntimeComponentName.VERIFICATION)

            instruction_runtime = InstructionRuntime(workspace_root=project_root, trace=trace)
            mark(RuntimeComponentName.INSTRUCTIONS)

            settings = config.to_settings()
            model_config = config.to_model_runtime_config()
            model_provider = OpenAICompatibleModelProvider(
                settings,
                timeout_seconds=model_config.request_timeout_seconds,
            )
            model_registry = ModelProviderRegistry(
                default_provider_name=model_config.default_provider
            )
            model_registry.register(model_provider)
            model_runtime = ModelRuntime(
                registry=model_registry,
                tool_registry=tools,
                config=model_config,
                trace=trace,
            )
            mark(RuntimeComponentName.MODEL)

            context_manager = ContextManager(
                system_prompt=SYSTEM_PROMPT,
                user_goal=user_goal,
                provider=None,
                model_runtime=model_runtime,
                run_id=identity.run_id,
                session_id=identity.session_id,
                task_id=identity.task_id,
                db_path=config.context_db_path(trace.store.run_dir),
                trace=trace,
            )
            mark(RuntimeComponentName.CONTEXT)

            protocol_runtime = ToolCallingProtocolRuntime(
                registry=tools,
                trace=trace,
                state_store=ToolProtocolStateStore(trace.store.run_dir / "tool_protocol.sqlite3"),
                workspace_state_hook=_workspace_state_context_hook(state_runtime),
            )

            planner = _create_or_resume_planner(
                workspace_root=project_root,
                session_id=config.resume_session,
                identity=identity,
                user_goal=user_goal,
                trace=trace,
                workspace_health=workspace_health or state_runtime.get_workspace_health(),
            )
            command_runtime.planner = planner
            mutation_runtime.planner = planner
            verification_runtime.planner = planner
            for runtime in (
                planner,
                model_runtime,
                command_runtime,
                sandbox_runtime,
                verification_runtime,
            ):
                setattr(runtime, "cancellation_token", None)
            mark(RuntimeComponentName.PLANNER)

            tool_runtime = ToolRuntime(
                registry=tools,
                policy=ToolPolicy.coding_agent(),
                trace=trace,
                workspace_root=project_root,
                planner=planner,
                policy_runtime=policy_runtime,
                approval_gate=approval_gate,
                dry_run=config.dry_run,
            )

            return RuntimeGraph(
                config=config,
                trace=trace,
                workspace_state=state_runtime,
                policy_runtime=policy_runtime,
                approval_gate=approval_gate,
                sandbox_runtime=sandbox_runtime,
                command_runtime=command_runtime,
                mutation_runtime=mutation_runtime,
                tools=tools,
                verification_runtime=verification_runtime,
                instruction_runtime=instruction_runtime,
                model_runtime=model_runtime,
                context_manager=context_manager,
                tool_runtime=tool_runtime,
                protocol_runtime=protocol_runtime,
                planner=planner,
                components=components,
            )
        except Exception as exc:
            for component, state in list(components.items()):
                if state == RuntimeComponentState.PENDING:
                    components[component] = RuntimeComponentState.FAILED
                    break
            raise RuntimeInitializationError(
                "Runtime graph initialization failed.",
                code="runtime_graph_failed",
                details={"error_type": type(exc).__name__, "message": str(exc)},
            ) from exc


def _create_or_resume_planner(
    *,
    workspace_root: Path,
    session_id: str | None,
    identity: RunIdentity,
    user_goal: str,
    trace: TraceRuntime,
    workspace_health: WorkspaceHealthReport,
) -> PlannerRuntime:
    planner = PlannerRuntime(
        workspace_root,
        session_id=session_id or identity.session_id,
        task_id=identity.task_id,
        trace=trace,
    )
    if session_id:
        return planner.resume(
            session_id,
            workspace_health=workspace_health.to_dict(),
        )
    planner.start_task(user_goal)
    return planner


def _workspace_state_context_hook(state_runtime: LocalWorkspaceStateRuntime):
    def hook(context, *, batch, tool_call_id: str | None) -> None:
        _ = batch, tool_call_id
        state_runtime.record_external_changes()
        context.add_workspace_state(state_runtime.get_workspace_health().to_observation())

    return hook
