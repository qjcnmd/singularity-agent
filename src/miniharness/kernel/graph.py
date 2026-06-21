from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from miniharness.command import CommandRuntime
from miniharness.config import ProductionRuntimeConfig
from miniharness.code_index import ProjectIndexRuntime
from miniharness.edit import EditRuntime
from miniharness.agent import SYSTEM_PROMPT
from miniharness.context import ContextManager
from miniharness.evaluation import EvaluationRuntime
from miniharness.instructions import InstructionRuntime
from miniharness.interaction import InteractionMode, InteractionRuntime
from miniharness.model import (
    ModelProviderRegistry,
    ModelRuntime,
    OpenAICompatibleModelProvider,
)
from miniharness.memory import MemoryRuntime
from miniharness.observability import TraceRuntime
from miniharness.policy import ApprovalGate, ApprovalMode, PolicyRuntime
from miniharness.plugins import PluginRuntime
from miniharness.planner import PlannerRuntime, create_or_resume_planner
from miniharness.review import ReviewRuntime
from miniharness.sandbox import SandboxRuntime
from miniharness.tool_protocol.runtime import ToolCallingProtocolRuntime
from miniharness.tool_protocol.state import ToolProtocolStateStore
from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.tools.command import register_command_tools
from miniharness.tools.code_index import register_code_index_tools
from miniharness.tools.edit import register_edit_tools
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
    RuntimeComponentName.INTERACTION,
    RuntimeComponentName.WORKSPACE_STATE,
    RuntimeComponentName.PROJECT_INDEX,
    RuntimeComponentName.MEMORY,
    RuntimeComponentName.POLICY,
    RuntimeComponentName.SANDBOX,
    RuntimeComponentName.COMMAND,
    RuntimeComponentName.MUTATION,
    RuntimeComponentName.EDIT,
    RuntimeComponentName.TOOLS,
    RuntimeComponentName.PLUGINS,
    RuntimeComponentName.TOOL_RUNTIME,
    RuntimeComponentName.TOOL_PROTOCOL,
    RuntimeComponentName.VERIFICATION,
    RuntimeComponentName.REVIEW,
    RuntimeComponentName.EVALUATION,
    RuntimeComponentName.INSTRUCTIONS,
    RuntimeComponentName.MODEL,
    RuntimeComponentName.CONTEXT,
    RuntimeComponentName.PLANNER,
]


@dataclass
class _RuntimeComponentMarker:
    components: dict[RuntimeComponentName, RuntimeComponentState]
    trace: TraceRuntime

    def mark(self, component: RuntimeComponentName) -> None:
        self.components[component] = RuntimeComponentState.READY
        self.trace.record(
            "runtime.initialized",
            {"component": component.value, "state": RuntimeComponentState.READY.value},
        )


@dataclass(frozen=True)
class _InfraRuntimes:
    interaction_runtime: InteractionRuntime
    state_runtime: LocalWorkspaceStateRuntime
    project_index_runtime: ProjectIndexRuntime
    memory_runtime: MemoryRuntime


@dataclass(frozen=True)
class _PolicySandboxRuntimes:
    policy_runtime: PolicyRuntime
    approval_gate: ApprovalGate
    sandbox_runtime: SandboxRuntime


@dataclass(frozen=True)
class _ExecutionCoreRuntimes:
    command_runtime: CommandRuntime
    mutation_runtime: MutationRuntime
    edit_runtime: EditRuntime


@dataclass(frozen=True)
class _ToolProtocolRuntimes:
    tools: ToolRegistry
    plugin_runtime: PluginRuntime
    tool_runtime: ToolRuntime
    protocol_runtime: ToolCallingProtocolRuntime


@dataclass(frozen=True)
class _VerificationReviewRuntimes:
    verification_runtime: VerificationRuntime
    review_runtime: ReviewRuntime


@dataclass(frozen=True)
class _ModelContextRuntimes:
    instruction_runtime: InstructionRuntime
    model_runtime: ModelRuntime
    context_manager: ContextManager


@dataclass
class RuntimeGraph:
    config: ProductionRuntimeConfig
    trace: TraceRuntime
    interaction_runtime: InteractionRuntime
    workspace_state: LocalWorkspaceStateRuntime
    project_index_runtime: ProjectIndexRuntime
    memory_runtime: MemoryRuntime
    policy_runtime: PolicyRuntime
    approval_gate: ApprovalGate
    sandbox_runtime: SandboxRuntime
    command_runtime: CommandRuntime
    mutation_runtime: MutationRuntime
    edit_runtime: EditRuntime
    tools: ToolRegistry
    plugin_runtime: PluginRuntime
    verification_runtime: VerificationRuntime
    review_runtime: ReviewRuntime
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
    _evaluation_runtime: EvaluationRuntime | None = field(default=None, repr=False)
    _evaluation_runtime_factory: Callable[[], EvaluationRuntime] | None = field(
        default=None,
        repr=False,
    )
    _cancellation_token_factory: Callable[[], Any] | None = field(default=None, repr=False)

    def __post_init__(self) -> None:
        if not self.components:
            self.components = {
                component: RuntimeComponentState.READY
                for component in self.initialization_order
            }

    def state(self, component: RuntimeComponentName) -> RuntimeComponentState:
        return self.components.get(component, RuntimeComponentState.PENDING)

    @property
    def evaluation_runtime(self) -> EvaluationRuntime:
        if self._evaluation_runtime is not None:
            return self._evaluation_runtime
        if self._evaluation_runtime_factory is None:
            raise RuntimeInitializationError(
                "Evaluation runtime is not available.",
                code="evaluation_runtime_unavailable",
            )
        try:
            runtime = self._evaluation_runtime_factory()
        except Exception as exc:
            self.components[RuntimeComponentName.EVALUATION] = RuntimeComponentState.FAILED
            raise RuntimeInitializationError(
                "Evaluation runtime initialization failed.",
                code="evaluation_runtime_failed",
                details={"error_type": type(exc).__name__, "message": str(exc)},
            ) from exc
        runtime.planner_runtime = self.planner
        if self._cancellation_token_factory is not None:
            setattr(runtime, "cancellation_token", self._cancellation_token_factory())
        else:
            setattr(runtime, "cancellation_token", None)
        self._evaluation_runtime = runtime
        self._evaluation_runtime_factory = None
        return runtime

    def cancellation_targets(self) -> list[tuple[str, Any]]:
        targets: list[tuple[str, Any]] = [
            ("planner", self.planner),
            ("model_runtime", self.model_runtime),
            ("command_runtime", self.command_runtime),
            ("sandbox_runtime", self.sandbox_runtime),
            ("verification_runtime", self.verification_runtime),
            ("edit_runtime", self.edit_runtime),
            ("review_runtime", self.review_runtime),
            ("tool_runtime", self.tool_runtime),
            ("protocol_runtime", self.protocol_runtime),
            ("context_manager", self.context_manager),
        ]
        if self._evaluation_runtime is not None:
            targets.append(("evaluation_runtime", self._evaluation_runtime))
        return targets

    def reset_cancellation_tokens(self) -> None:
        self._cancellation_token_factory = None
        for _name, runtime in self.cancellation_targets():
            setattr(runtime, "cancellation_token", None)

    def install_cancellation_tokens(self, token_factory: Callable[[], Any]) -> None:
        self._cancellation_token_factory = token_factory
        for _name, runtime in self.cancellation_targets():
            setattr(runtime, "cancellation_token", token_factory())

    def components_for_health(self) -> dict[str, Any]:
        return {
            "config": self.config,
            "trace": self.trace,
            "interaction": self.interaction_runtime,
            "workspace": self.workspace_state,
            "project_index": self.project_index_runtime,
            "memory": self.memory_runtime,
            "policy": self.policy_runtime,
            "sandbox": self.sandbox_runtime,
            "command": self.command_runtime,
            "mutation": self.mutation_runtime,
            "edit": self.edit_runtime,
            "tools": self.tools,
            "plugins": self.plugin_runtime,
            "tool_runtime": self.tool_runtime,
            "tool_protocol": self.protocol_runtime,
            "verification": self.verification_runtime,
            "review": self.review_runtime,
            "evaluation": self._evaluation_runtime or self._evaluation_runtime_factory,
            "instructions": self.instruction_runtime,
            "model": self.model_runtime,
            "context": self.context_manager,
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
        interaction_runtime: InteractionRuntime | None = None,
    ) -> RuntimeGraph:
        components = {
            component: RuntimeComponentState.PENDING
            for component in RUNTIME_INITIALIZATION_ORDER
        }
        marker = _RuntimeComponentMarker(components=components, trace=trace)

        try:
            marker.mark(RuntimeComponentName.CONFIGURATION)
            marker.mark(RuntimeComponentName.OBSERVABILITY)
            infra = self._build_infra(
                project_root=project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                interaction_runtime=interaction_runtime,
                marker=marker,
            )
            policy_sandbox = self._build_policy_sandbox(
                project_root=project_root,
                config=config,
                trace=trace,
                interaction_runtime=infra.interaction_runtime,
                marker=marker,
            )
            execution_core = self._build_execution_core(
                project_root=project_root,
                trace=trace,
                infra=infra,
                policy_sandbox=policy_sandbox,
                marker=marker,
            )
            tool_protocol = self._build_tools_protocol(
                project_root=project_root,
                config=config,
                trace=trace,
                infra=infra,
                policy_sandbox=policy_sandbox,
                execution_core=execution_core,
                marker=marker,
            )
            verification_review = self._build_verification_review(
                project_root=project_root,
                trace=trace,
                infra=infra,
                policy_sandbox=policy_sandbox,
                execution_core=execution_core,
                tool_protocol=tool_protocol,
                marker=marker,
            )
            marker.mark(RuntimeComponentName.EVALUATION)
            model_context = self._build_model_context(
                project_root=project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                execution_core=execution_core,
                tool_protocol=tool_protocol,
                verification_review=verification_review,
                marker=marker,
            )
            planner = self._create_planner(
                project_root=project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                workspace_health=workspace_health,
                state_runtime=infra.state_runtime,
            )
            self._wire_planner(
                planner=planner,
                execution_core=execution_core,
                tool_protocol=tool_protocol,
                verification_review=verification_review,
            )
            self._prime_planner_context(
                user_goal=user_goal,
                planner=planner,
                project_index_runtime=infra.project_index_runtime,
                memory_runtime=infra.memory_runtime,
                context_manager=model_context.context_manager,
            )

            graph = RuntimeGraph(
                config=config,
                trace=trace,
                interaction_runtime=infra.interaction_runtime,
                workspace_state=infra.state_runtime,
                project_index_runtime=infra.project_index_runtime,
                memory_runtime=infra.memory_runtime,
                policy_runtime=policy_sandbox.policy_runtime,
                approval_gate=policy_sandbox.approval_gate,
                sandbox_runtime=policy_sandbox.sandbox_runtime,
                command_runtime=execution_core.command_runtime,
                mutation_runtime=execution_core.mutation_runtime,
                edit_runtime=execution_core.edit_runtime,
                tools=tool_protocol.tools,
                plugin_runtime=tool_protocol.plugin_runtime,
                verification_runtime=verification_review.verification_runtime,
                review_runtime=verification_review.review_runtime,
                instruction_runtime=model_context.instruction_runtime,
                model_runtime=model_context.model_runtime,
                context_manager=model_context.context_manager,
                tool_runtime=tool_protocol.tool_runtime,
                protocol_runtime=tool_protocol.protocol_runtime,
                planner=planner,
                components=components,
                _evaluation_runtime_factory=self._evaluation_runtime_factory(
                    project_root=project_root,
                    trace=trace,
                    infra=infra,
                    execution_core=execution_core,
                    tool_protocol=tool_protocol,
                    verification_review=verification_review,
                    planner=planner,
                ),
            )
            graph.reset_cancellation_tokens()
            marker.mark(RuntimeComponentName.PLANNER)
            return graph
        except Exception as exc:
            self._mark_first_pending_failed(components)
            raise RuntimeInitializationError(
                "Runtime graph initialization failed.",
                code="runtime_graph_failed",
                details={"error_type": type(exc).__name__, "message": str(exc)},
            ) from exc

    def _build_infra(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        identity: RunIdentity,
        user_goal: str,
        interaction_runtime: InteractionRuntime | None,
        marker: _RuntimeComponentMarker,
    ) -> _InfraRuntimes:
        if interaction_runtime is None:
            interaction_mode = (
                InteractionMode.NON_INTERACTIVE
                if config.approval_mode == ApprovalMode.NON_INTERACTIVE
                else config.interaction_mode
            )
            interaction_runtime = InteractionRuntime(
                mode=interaction_mode,
                trace=trace,
            )
        if hasattr(trace, "set_interaction_sink"):
            trace.set_interaction_sink(interaction_runtime.consume_trace_event)
        marker.mark(RuntimeComponentName.INTERACTION)

        state_runtime = LocalWorkspaceStateRuntime(project_root, trace=trace)
        if config.resume_session:
            state_runtime.recover_session(config.resume_session)
        else:
            state_runtime.begin_session(task_id=identity.task_id, session_id=identity.session_id)
        marker.mark(RuntimeComponentName.WORKSPACE_STATE)

        project_index_runtime = ProjectIndexRuntime(
            project_root,
            trace=trace,
            config=config.to_project_index_config(),
        )
        if config.project_index_enabled:
            project_index_runtime.bootstrap(reason="kernel_boot")
        marker.mark(RuntimeComponentName.PROJECT_INDEX)

        memory_runtime = MemoryRuntime(project_root, trace=trace)
        memory_runtime.start_session(session_id=identity.session_id, user_goal=user_goal)
        marker.mark(RuntimeComponentName.MEMORY)

        return _InfraRuntimes(
            interaction_runtime=interaction_runtime,
            state_runtime=state_runtime,
            project_index_runtime=project_index_runtime,
            memory_runtime=memory_runtime,
        )

    def _build_policy_sandbox(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        interaction_runtime: InteractionRuntime,
        marker: _RuntimeComponentMarker,
    ) -> _PolicySandboxRuntimes:
        policy_config = config.to_policy_config()
        policy_runtime = PolicyRuntime(policy_config, trace=trace)
        approval_gate = ApprovalGate(
            policy_config,
            trace=trace,
            interaction=interaction_runtime,
        )
        marker.mark(RuntimeComponentName.POLICY)

        sandbox_runtime = SandboxRuntime(
            project_root,
            trace=trace,
            security_mode=config.security_mode,
        )
        marker.mark(RuntimeComponentName.SANDBOX)

        return _PolicySandboxRuntimes(
            policy_runtime=policy_runtime,
            approval_gate=approval_gate,
            sandbox_runtime=sandbox_runtime,
        )

    def _build_execution_core(
        self,
        *,
        project_root: Path,
        trace: TraceRuntime,
        infra: _InfraRuntimes,
        policy_sandbox: _PolicySandboxRuntimes,
        marker: _RuntimeComponentMarker,
    ) -> _ExecutionCoreRuntimes:
        command_runtime = CommandRuntime(
            project_root,
            trace=trace,
            state_runtime=infra.state_runtime,
            planner=None,
            policy_runtime=policy_sandbox.policy_runtime,
            sandbox_runtime=policy_sandbox.sandbox_runtime,
        )
        marker.mark(RuntimeComponentName.COMMAND)

        mutation_runtime = MutationRuntime(
            project_root,
            trace=trace,
            state_runtime=infra.state_runtime,
            planner=None,
            policy_runtime=policy_sandbox.policy_runtime,
            project_index_runtime=infra.project_index_runtime,
        )
        marker.mark(RuntimeComponentName.MUTATION)

        edit_runtime = EditRuntime(
            project_root,
            mutation_runtime=mutation_runtime,
            project_index_runtime=infra.project_index_runtime,
            trace=trace,
        )
        marker.mark(RuntimeComponentName.EDIT)

        return _ExecutionCoreRuntimes(
            command_runtime=command_runtime,
            mutation_runtime=mutation_runtime,
            edit_runtime=edit_runtime,
        )

    def _build_tools_protocol(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        infra: _InfraRuntimes,
        policy_sandbox: _PolicySandboxRuntimes,
        execution_core: _ExecutionCoreRuntimes,
        marker: _RuntimeComponentMarker,
    ) -> _ToolProtocolRuntimes:
        tools = ToolRegistry(project_root)
        register_mutation_tools(tools, execution_core.mutation_runtime)
        register_edit_tools(tools, execution_core.edit_runtime)
        register_command_tools(tools, execution_core.command_runtime)
        register_workspace_state_tools(tools, infra.state_runtime)
        register_code_index_tools(tools, infra.project_index_runtime)
        marker.mark(RuntimeComponentName.TOOLS)

        plugin_runtime = PluginRuntime(project_root, trace=trace)
        plugin_runtime.activate(
            registry=tools,
            policy_runtime=policy_sandbox.policy_runtime,
        )
        marker.mark(RuntimeComponentName.PLUGINS)

        tool_runtime = ToolRuntime(
            registry=tools,
            policy=ToolPolicy.coding_agent(),
            trace=trace,
            workspace_root=project_root,
            planner=None,
            policy_runtime=policy_sandbox.policy_runtime,
            approval_gate=policy_sandbox.approval_gate,
            dry_run=config.dry_run,
        )
        marker.mark(RuntimeComponentName.TOOL_RUNTIME)

        protocol_runtime = ToolCallingProtocolRuntime(
            registry=tools,
            trace=trace,
            state_store=ToolProtocolStateStore(trace.store.run_dir / "tool_protocol.sqlite3"),
            workspace_state_hook=_workspace_state_context_hook(infra.state_runtime),
        )
        marker.mark(RuntimeComponentName.TOOL_PROTOCOL)

        return _ToolProtocolRuntimes(
            tools=tools,
            plugin_runtime=plugin_runtime,
            tool_runtime=tool_runtime,
            protocol_runtime=protocol_runtime,
        )

    def _build_verification_review(
        self,
        *,
        project_root: Path,
        trace: TraceRuntime,
        infra: _InfraRuntimes,
        policy_sandbox: _PolicySandboxRuntimes,
        execution_core: _ExecutionCoreRuntimes,
        tool_protocol: _ToolProtocolRuntimes,
        marker: _RuntimeComponentMarker,
    ) -> _VerificationReviewRuntimes:
        verification_runtime = VerificationRuntime(
            project_root,
            command_runtime=execution_core.command_runtime,
            trace=trace,
            planner=None,
            policy_runtime=policy_sandbox.policy_runtime,
            project_index_runtime=infra.project_index_runtime,
        )
        register_verification_tools(tool_protocol.tools, verification_runtime)
        execution_core.edit_runtime.verification_runtime = verification_runtime
        marker.mark(RuntimeComponentName.VERIFICATION)

        review_runtime = ReviewRuntime(
            project_root,
            trace=trace,
            project_index_runtime=infra.project_index_runtime,
            policy_runtime=policy_sandbox.policy_runtime,
            model_runtime=None,
            memory_runtime=infra.memory_runtime,
        )
        execution_core.edit_runtime.review_runtime = review_runtime
        verification_runtime.review_runtime = review_runtime
        verification_runtime.memory_runtime = infra.memory_runtime
        marker.mark(RuntimeComponentName.REVIEW)

        return _VerificationReviewRuntimes(
            verification_runtime=verification_runtime,
            review_runtime=review_runtime,
        )

    def _build_model_context(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        identity: RunIdentity,
        user_goal: str,
        execution_core: _ExecutionCoreRuntimes,
        tool_protocol: _ToolProtocolRuntimes,
        verification_review: _VerificationReviewRuntimes,
        marker: _RuntimeComponentMarker,
    ) -> _ModelContextRuntimes:
        instruction_runtime = InstructionRuntime(workspace_root=project_root, trace=trace)
        marker.mark(RuntimeComponentName.INSTRUCTIONS)

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
            tool_registry=tool_protocol.tools,
            config=model_config,
            trace=trace,
        )
        verification_review.review_runtime.model_runtime = model_runtime
        marker.mark(RuntimeComponentName.MODEL)

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
        execution_core.edit_runtime.context_manager = context_manager
        marker.mark(RuntimeComponentName.CONTEXT)

        return _ModelContextRuntimes(
            instruction_runtime=instruction_runtime,
            model_runtime=model_runtime,
            context_manager=context_manager,
        )

    def _create_planner(
        self,
        *,
        project_root: Path,
        config: ProductionRuntimeConfig,
        trace: TraceRuntime,
        identity: RunIdentity,
        user_goal: str,
        workspace_health: WorkspaceHealthReport | None,
        state_runtime: LocalWorkspaceStateRuntime,
    ) -> PlannerRuntime:
        return create_or_resume_planner(
            workspace_root=project_root,
            session_id=config.resume_session,
            task_id=identity.task_id,
            user_goal=user_goal,
            trace=trace,
            workspace_health=workspace_health or state_runtime.get_workspace_health(),
            fallback_session_id=identity.session_id,
        )

    @staticmethod
    def _wire_planner(
        *,
        planner: PlannerRuntime,
        execution_core: _ExecutionCoreRuntimes,
        tool_protocol: _ToolProtocolRuntimes,
        verification_review: _VerificationReviewRuntimes,
    ) -> None:
        execution_core.command_runtime.planner = planner
        execution_core.mutation_runtime.planner = planner
        verification_review.verification_runtime.planner = planner
        execution_core.edit_runtime.planner = planner
        verification_review.review_runtime.planner = planner
        tool_protocol.tool_runtime.planner = planner

    @staticmethod
    def _prime_planner_context(
        *,
        user_goal: str,
        planner: PlannerRuntime,
        project_index_runtime: ProjectIndexRuntime,
        memory_runtime: MemoryRuntime,
        context_manager: ContextManager,
    ) -> None:
        index_observation = project_index_runtime.observation_for_goal(user_goal)
        context_manager.add_project_index(index_observation)
        planner.record_project_index_observation(index_observation)
        memory_block = memory_runtime.context_block(
            goal=user_goal,
            max_items=6,
            token_budget=512,
        )
        if memory_block.items:
            context_manager.add_memory_context_block(memory_block)

    @staticmethod
    def _evaluation_runtime_factory(
        *,
        project_root: Path,
        trace: TraceRuntime,
        infra: _InfraRuntimes,
        execution_core: _ExecutionCoreRuntimes,
        tool_protocol: _ToolProtocolRuntimes,
        verification_review: _VerificationReviewRuntimes,
        planner: PlannerRuntime,
    ) -> Callable[[], EvaluationRuntime]:
        def build_evaluation_runtime() -> EvaluationRuntime:
            return EvaluationRuntime(
                project_root=project_root,
                trace_runtime=trace,
                verification_runtime=verification_review.verification_runtime,
                memory_runtime=infra.memory_runtime,
                planner_runtime=planner,
                tool_runtime=tool_protocol.tool_runtime,
                command_runtime=execution_core.command_runtime,
                mutation_runtime=execution_core.mutation_runtime,
            )

        return build_evaluation_runtime

    @staticmethod
    def _mark_first_pending_failed(
        components: dict[RuntimeComponentName, RuntimeComponentState],
    ) -> None:
        for component, state in list(components.items()):
            if state == RuntimeComponentState.PENDING:
                components[component] = RuntimeComponentState.FAILED
                break


def _workspace_state_context_hook(state_runtime: LocalWorkspaceStateRuntime):
    def hook(context, *, batch, tool_call_id: str | None) -> None:
        _ = batch, tool_call_id
        state_runtime.record_external_changes()
        context.add_workspace_state(state_runtime.get_workspace_health().to_observation())

    return hook
