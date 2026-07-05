from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from singularity.agent_loop import SYSTEM_PROMPT
from singularity.code_index import ProjectIndex
from singularity.command import CommandExecutor
from singularity.config import ProductionConfig
from singularity.context import ContextManager
from singularity.edit import EditExecutor
from singularity.evaluation.harness import EvaluationHarness
from singularity.instructions import PromptAssemblyPipeline
from singularity.interaction import InteractionController
from singularity.kernel.exceptions import AgentGraphInitializationError
from singularity.kernel.models import (
    ComponentName,
    ComponentState,
    RunIdentity,
)
from singularity.memory import MemoryLearningPipeline
from singularity.model import (
    ModelProviderRegistry,
    ModelRunner,
    OpenAICompatibleModelProvider,
)
from singularity.observability import TraceRecorder
from singularity.planner import Planner, create_or_resume_planner
from singularity.plugins import PluginManager
from singularity.policy import ApprovalGate, PolicyConfig, PolicyEngine
from singularity.review import ReviewPipeline
from singularity.sandbox import SandboxManager
from singularity.session.models import RecoveryGateDecision
from singularity.tool_protocol.engine import ToolProtocolEngine
from singularity.tool_protocol.state import ToolProtocolStateStore
from singularity.tools import ToolExecutor, ToolPolicy, ToolRegistry
from singularity.tools.code_index import register_code_index_tools
from singularity.tools.command import register_command_tools
from singularity.tools.edit import register_edit_tools
from singularity.tools.mutation import register_mutation_tools
from singularity.tools.verification import register_verification_tools
from singularity.tools.workspace_state import register_workspace_state_tools
from singularity.verification.runner import VerificationRunner
from singularity.workspace import WorkspaceMutationManager
from singularity.workspace_state import WorkspaceHealthReport, WorkspaceStateManager

AGENT_COMPONENT_INITIALIZATION_ORDER = [
    ComponentName.CONFIGURATION,
    ComponentName.OBSERVABILITY,
    ComponentName.INTERACTION,
    ComponentName.WORKSPACE_STATE,
    ComponentName.PROJECT_INDEX,
    ComponentName.MEMORY,
    ComponentName.POLICY,
    ComponentName.SANDBOX,
    ComponentName.COMMAND,
    ComponentName.MUTATION,
    ComponentName.EDIT,
    ComponentName.TOOLS,
    ComponentName.PLUGINS,
    ComponentName.TOOL_EXECUTOR,
    ComponentName.TOOL_PROTOCOL,
    ComponentName.VERIFICATION,
    ComponentName.REVIEW,
    ComponentName.EVALUATION,
    ComponentName.INSTRUCTIONS,
    ComponentName.MODEL,
    ComponentName.CONTEXT,
    ComponentName.PLANNER,
]


@dataclass
class _ComponentMarker:
    components: dict[ComponentName, ComponentState]
    trace: TraceRecorder

    def mark(self, component: ComponentName) -> None:
        self.components[component] = ComponentState.READY
        self.trace.record(
            "component.initialized",
            {"component": component.value, "state": ComponentState.READY.value},
        )


@dataclass(frozen=True)
class _InfraComponents:
    interaction_controller: InteractionController
    workspace_state_manager: WorkspaceStateManager
    project_index: ProjectIndex
    memory_pipeline: MemoryLearningPipeline


@dataclass(frozen=True)
class _PolicySandboxComponents:
    policy_engine: PolicyEngine
    approval_gate: ApprovalGate
    sandbox_manager: SandboxManager


@dataclass(frozen=True)
class _ExecutionCoreComponents:
    command_executor: CommandExecutor
    mutation_manager: WorkspaceMutationManager
    edit_executor: EditExecutor


@dataclass(frozen=True)
class _ToolProtocolEngines:
    tools: ToolRegistry
    plugin_manager: PluginManager
    tool_executor: ToolExecutor
    tool_protocol: ToolProtocolEngine


@dataclass(frozen=True)
class _VerificationReviewPipelines:
    verification_runner: VerificationRunner
    review_pipeline: ReviewPipeline


@dataclass(frozen=True)
class _ModelContextComponents:
    prompt_assembly: PromptAssemblyPipeline
    model_runner: ModelRunner
    context_manager: ContextManager


@dataclass
class AgentGraph:
    config: ProductionConfig
    trace: TraceRecorder
    interaction_controller: InteractionController
    workspace_state: WorkspaceStateManager
    project_index: ProjectIndex
    memory_pipeline: MemoryLearningPipeline
    policy_engine: PolicyEngine
    approval_gate: ApprovalGate
    sandbox_manager: SandboxManager
    command_executor: CommandExecutor
    mutation_manager: WorkspaceMutationManager
    edit_executor: EditExecutor
    tools: ToolRegistry
    plugin_manager: PluginManager
    verification_runner: VerificationRunner
    review_pipeline: ReviewPipeline
    prompt_assembly: PromptAssemblyPipeline
    model_runner: ModelRunner
    context_manager: ContextManager
    tool_executor: ToolExecutor
    tool_protocol: ToolProtocolEngine
    planner: Planner
    recovery_gate_decision: RecoveryGateDecision | None = None
    initialization_order: list[ComponentName] = field(
        default_factory=lambda: list(AGENT_COMPONENT_INITIALIZATION_ORDER)
    )
    components: dict[ComponentName, ComponentState] = field(default_factory=dict)
    _evaluation_harness: EvaluationHarness | None = field(default=None, repr=False)
    _evaluation_harness_factory: Callable[[], EvaluationHarness] | None = field(
        default=None,
        repr=False,
    )
    _cancellation_token_factory: Callable[[], Any] | None = field(default=None, repr=False)

    def __post_init__(self) -> None:
        if not self.components:
            self.components = {
                component: ComponentState.READY
                for component in self.initialization_order
            }

    def state(self, component: ComponentName) -> ComponentState:
        return self.components.get(component, ComponentState.PENDING)

    @property
    def evaluation_harness(self) -> EvaluationHarness:
        if self._evaluation_harness is not None:
            return self._evaluation_harness
        if self._evaluation_harness_factory is None:
            raise AgentGraphInitializationError(
                "EvaluationHarness is not available.",
                code="evaluation_harness_unavailable",
            )
        try:
            evaluation_harness = self._evaluation_harness_factory()
        except Exception as exc:
            self.components[ComponentName.EVALUATION] = ComponentState.FAILED
            raise AgentGraphInitializationError(
                "EvaluationHarness initialization failed.",
                code="evaluation_harness_failed",
                details={"error_type": type(exc).__name__, "message": str(exc)},
            ) from exc
        evaluation_harness.planner = self.planner
        if self._cancellation_token_factory is not None:
            evaluation_harness.cancellation_token = self._cancellation_token_factory()
        else:
            evaluation_harness.cancellation_token = None
        self._evaluation_harness = evaluation_harness
        self._evaluation_harness_factory = None
        return evaluation_harness

    def cancellation_targets(self) -> list[tuple[str, Any]]:
        targets: list[tuple[str, Any]] = [
            ("planner", self.planner),
            ("model_runner", self.model_runner),
            ("command_executor", self.command_executor),
            ("sandbox_manager", self.sandbox_manager),
            ("verification_runner", self.verification_runner),
            ("edit_executor", self.edit_executor),
            ("review_pipeline", self.review_pipeline),
            ("tool_executor", self.tool_executor),
            ("tool_protocol", self.tool_protocol),
            ("context_manager", self.context_manager),
        ]
        if self._evaluation_harness is not None:
            targets.append(("evaluation_harness", self._evaluation_harness))
        return targets

    def reset_cancellation_tokens(self) -> None:
        self._cancellation_token_factory = None
        for _name, component in self.cancellation_targets():
            component.cancellation_token = None

    def install_cancellation_tokens(self, token_factory: Callable[[], Any]) -> None:
        self._cancellation_token_factory = token_factory
        for _name, component in self.cancellation_targets():
            component.cancellation_token = token_factory()

    def components_for_health(self) -> dict[str, Any]:
        return {
            "config": self.config,
            "trace": self.trace,
            "interaction": self.interaction_controller,
            "workspace": self.workspace_state,
            "project_index": self.project_index,
            "memory": self.memory_pipeline,
            "policy": self.policy_engine,
            "sandbox": self.sandbox_manager,
            "command": self.command_executor,
            "mutation": self.mutation_manager,
            "edit": self.edit_executor,
            "tools": self.tools,
            "plugins": self.plugin_manager,
            "tool_executor": self.tool_executor,
            "tool_protocol": self.tool_protocol,
            "verification": self.verification_runner,
            "review": self.review_pipeline,
            "evaluation": self._evaluation_harness or self._evaluation_harness_factory,
            "instructions": self.prompt_assembly,
            "model": self.model_runner,
            "context": self.context_manager,
            "planner": self.planner,
        }


class AgentGraphBuilder:
    def build(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        identity: RunIdentity,
        user_goal: str,
        workspace_health: WorkspaceHealthReport | None = None,
        recovery_gate_decision: RecoveryGateDecision | None = None,
        interaction_controller: InteractionController | None = None,
    ) -> AgentGraph:
        components = {
            component: ComponentState.PENDING
            for component in AGENT_COMPONENT_INITIALIZATION_ORDER
        }
        marker = _ComponentMarker(components=components, trace=trace)

        try:
            marker.mark(ComponentName.CONFIGURATION)
            marker.mark(ComponentName.OBSERVABILITY)
            infra = self._build_infra(
                project_root=project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                interaction_controller=interaction_controller,
                marker=marker,
            )
            policy_sandbox = self._build_policy_sandbox(
                project_root=project_root,
                config=config,
                trace=trace,
                interaction_controller=infra.interaction_controller,
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
            marker.mark(ComponentName.EVALUATION)
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
                recovery_gate_decision=recovery_gate_decision,
            )
            planner = self._create_planner(
                project_root=project_root,
                config=config,
                trace=trace,
                identity=identity,
                user_goal=user_goal,
                workspace_health=workspace_health,
                workspace_state_manager=infra.workspace_state_manager,
            )
            self._wire_planner(
                planner=planner,
                config=config,
                infra=infra,
                policy_sandbox=policy_sandbox,
                execution_core=execution_core,
                tool_protocol=tool_protocol,
                verification_review=verification_review,
                model_runner=model_context.model_runner,
            )
            self._prime_planner_context(
                user_goal=user_goal,
                planner=planner,
                recovery_gate_decision=recovery_gate_decision,
                project_index=infra.project_index,
                memory_pipeline=infra.memory_pipeline,
                context_manager=model_context.context_manager,
            )

            graph = AgentGraph(
                config=config,
                trace=trace,
                interaction_controller=infra.interaction_controller,
                workspace_state=infra.workspace_state_manager,
                project_index=infra.project_index,
                memory_pipeline=infra.memory_pipeline,
                policy_engine=policy_sandbox.policy_engine,
                approval_gate=policy_sandbox.approval_gate,
                sandbox_manager=policy_sandbox.sandbox_manager,
                command_executor=execution_core.command_executor,
                mutation_manager=execution_core.mutation_manager,
                edit_executor=execution_core.edit_executor,
                tools=tool_protocol.tools,
                plugin_manager=tool_protocol.plugin_manager,
                verification_runner=verification_review.verification_runner,
                review_pipeline=verification_review.review_pipeline,
                prompt_assembly=model_context.prompt_assembly,
                model_runner=model_context.model_runner,
                context_manager=model_context.context_manager,
                tool_executor=tool_protocol.tool_executor,
                tool_protocol=tool_protocol.tool_protocol,
                planner=planner,
                recovery_gate_decision=recovery_gate_decision,
                components=components,
                _evaluation_harness_factory=self._evaluation_harness_factory(
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
            marker.mark(ComponentName.PLANNER)
            return graph
        except Exception as exc:
            self._mark_first_pending_failed(components)
            raise AgentGraphInitializationError(
                "Agent graph initialization failed.",
                code="agent_graph_failed",
                details={"error_type": type(exc).__name__, "message": str(exc)},
            ) from exc

    def _build_infra(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        identity: RunIdentity,
        user_goal: str,
        interaction_controller: InteractionController | None,
        marker: _ComponentMarker,
    ) -> _InfraComponents:
        if interaction_controller is None:
            interaction_controller = InteractionController(
                mode=config.interaction_mode,
                trace=trace,
            )
        if hasattr(trace, "set_interaction_sink"):
            trace.set_interaction_sink(interaction_controller.consume_trace_event)
        marker.mark(ComponentName.INTERACTION)

        workspace_state_manager = WorkspaceStateManager(project_root, trace=trace)
        if config.resume_session:
            workspace_state_manager.recover_session(config.resume_session)
        else:
            workspace_state_manager.begin_session(task_id=identity.task_id, session_id=identity.session_id)
        marker.mark(ComponentName.WORKSPACE_STATE)

        project_index = ProjectIndex(
            project_root,
            trace=trace,
            config=config.to_project_index_config(),
        )
        if config.project_index_enabled:
            project_index.bootstrap(reason="kernel_boot")
        marker.mark(ComponentName.PROJECT_INDEX)

        memory_pipeline = MemoryLearningPipeline(project_root, trace=trace)
        memory_pipeline.start_session(session_id=identity.session_id, user_goal=user_goal)
        marker.mark(ComponentName.MEMORY)

        return _InfraComponents(
            interaction_controller=interaction_controller,
            workspace_state_manager=workspace_state_manager,
            project_index=project_index,
            memory_pipeline=memory_pipeline,
        )

    def _build_policy_sandbox(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        interaction_controller: InteractionController,
        marker: _ComponentMarker,
    ) -> _PolicySandboxComponents:
        permission_profile = config.to_permission_profile()
        policy_config = PolicyConfig(
            workspace_root=project_root,
            permission_profile=permission_profile,
        )
        policy_engine = PolicyEngine(policy_config, trace=trace)
        approval_gate = ApprovalGate(
            policy_config,
            trace=trace,
            interaction=interaction_controller,
        )
        marker.mark(ComponentName.POLICY)

        sandbox_manager = SandboxManager(
            project_root,
            trace=trace,
            permission_profile=permission_profile,
        )
        marker.mark(ComponentName.SANDBOX)

        return _PolicySandboxComponents(
            policy_engine=policy_engine,
            approval_gate=approval_gate,
            sandbox_manager=sandbox_manager,
        )

    def _build_execution_core(
        self,
        *,
        project_root: Path,
        trace: TraceRecorder,
        infra: _InfraComponents,
        policy_sandbox: _PolicySandboxComponents,
        marker: _ComponentMarker,
    ) -> _ExecutionCoreComponents:
        command_executor = CommandExecutor(
            project_root,
            trace=trace,
            workspace_state_manager=infra.workspace_state_manager,
            planner=None,
            policy_engine=policy_sandbox.policy_engine,
            approval_gate=policy_sandbox.approval_gate,
            sandbox_manager=policy_sandbox.sandbox_manager,
        )
        marker.mark(ComponentName.COMMAND)

        mutation_manager = WorkspaceMutationManager(
            project_root,
            trace=trace,
            workspace_state_manager=infra.workspace_state_manager,
            planner=None,
            policy_engine=policy_sandbox.policy_engine,
            approval_gate=policy_sandbox.approval_gate,
            project_index=infra.project_index,
        )
        marker.mark(ComponentName.MUTATION)

        edit_executor = EditExecutor(
            project_root,
            mutation_manager=mutation_manager,
            project_index=infra.project_index,
            trace=trace,
        )
        marker.mark(ComponentName.EDIT)

        return _ExecutionCoreComponents(
            command_executor=command_executor,
            mutation_manager=mutation_manager,
            edit_executor=edit_executor,
        )

    def _build_tools_protocol(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        infra: _InfraComponents,
        policy_sandbox: _PolicySandboxComponents,
        execution_core: _ExecutionCoreComponents,
        marker: _ComponentMarker,
    ) -> _ToolProtocolEngines:
        tools = ToolRegistry(
            project_root,
            permission_profile=policy_sandbox.policy_engine.config.permission_profile,
        )
        register_mutation_tools(tools, execution_core.mutation_manager)
        register_edit_tools(tools, execution_core.edit_executor)
        register_command_tools(tools, execution_core.command_executor)
        register_workspace_state_tools(tools, infra.workspace_state_manager)
        register_code_index_tools(tools, infra.project_index)
        marker.mark(ComponentName.TOOLS)

        plugin_manager = PluginManager(project_root, trace=trace)
        plugin_manager.activate(
            registry=tools,
            policy_engine=policy_sandbox.policy_engine,
        )
        marker.mark(ComponentName.PLUGINS)

        tool_executor = ToolExecutor(
            registry=tools,
            policy=ToolPolicy.coding_agent(),
            trace=trace,
            workspace_root=project_root,
            planner=None,
            policy_engine=policy_sandbox.policy_engine,
            approval_gate=policy_sandbox.approval_gate,
            dry_run=config.dry_run,
        )
        marker.mark(ComponentName.TOOL_EXECUTOR)

        tool_protocol = ToolProtocolEngine(
            registry=tools,
            trace=trace,
            state_store=ToolProtocolStateStore(trace.store.run_dir / "tool_protocol.sqlite3"),
            workspace_state_hook=_workspace_state_context_hook(infra.workspace_state_manager),
        )
        marker.mark(ComponentName.TOOL_PROTOCOL)

        return _ToolProtocolEngines(
            tools=tools,
            plugin_manager=plugin_manager,
            tool_executor=tool_executor,
            tool_protocol=tool_protocol,
        )

    def _build_verification_review(
        self,
        *,
        project_root: Path,
        trace: TraceRecorder,
        infra: _InfraComponents,
        policy_sandbox: _PolicySandboxComponents,
        execution_core: _ExecutionCoreComponents,
        tool_protocol: _ToolProtocolEngines,
        marker: _ComponentMarker,
    ) -> _VerificationReviewPipelines:
        verification_runner = VerificationRunner(
            project_root,
            command_executor=execution_core.command_executor,
            trace=trace,
            planner=None,
            policy_engine=policy_sandbox.policy_engine,
            approval_gate=policy_sandbox.approval_gate,
            project_index=infra.project_index,
        )
        register_verification_tools(tool_protocol.tools, verification_runner)
        execution_core.edit_executor.verification_runner = verification_runner
        marker.mark(ComponentName.VERIFICATION)

        review_pipeline = ReviewPipeline(
            project_root,
            trace=trace,
            project_index=infra.project_index,
            policy_engine=policy_sandbox.policy_engine,
            model_runner=None,
            memory_pipeline=infra.memory_pipeline,
        )
        execution_core.edit_executor.review_pipeline = review_pipeline
        verification_runner.review_pipeline = review_pipeline
        verification_runner.memory_pipeline = infra.memory_pipeline
        marker.mark(ComponentName.REVIEW)

        return _VerificationReviewPipelines(
            verification_runner=verification_runner,
            review_pipeline=review_pipeline,
        )

    def _build_model_context(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        identity: RunIdentity,
        user_goal: str,
        execution_core: _ExecutionCoreComponents,
        tool_protocol: _ToolProtocolEngines,
        verification_review: _VerificationReviewPipelines,
        marker: _ComponentMarker,
        recovery_gate_decision: RecoveryGateDecision | None = None,
    ) -> _ModelContextComponents:
        prompt_assembly = PromptAssemblyPipeline(workspace_root=project_root, trace=trace)
        marker.mark(ComponentName.INSTRUCTIONS)

        settings = config.to_settings()
        model_config = config.to_model_runner_config()
        model_provider = OpenAICompatibleModelProvider(
            settings,
            timeout_seconds=model_config.request_timeout_seconds,
        )
        model_registry = ModelProviderRegistry(
            default_provider_name=model_config.default_provider
        )
        model_registry.register(model_provider)
        model_runner = ModelRunner(
            registry=model_registry,
            tool_registry=tool_protocol.tools,
            config=model_config,
            trace=trace,
        )
        verification_review.review_pipeline.model_runner = model_runner
        marker.mark(ComponentName.MODEL)

        context_manager = ContextManager(
            system_prompt=SYSTEM_PROMPT,
            user_goal=user_goal,
            provider=None,
            model_runner=model_runner,
            run_id=identity.run_id,
            session_id=identity.session_id,
            task_id=identity.task_id,
            db_path=config.context_db_path(trace.store.run_dir),
            trace=trace,
        )
        if recovery_gate_decision is not None and recovery_gate_decision.mode != "new":
            context_manager.seed_session_resume_context(
                recovery_gate_decision.resume_context.to_model_context()
            )
        execution_core.edit_executor.context_manager = context_manager
        marker.mark(ComponentName.CONTEXT)

        return _ModelContextComponents(
            prompt_assembly=prompt_assembly,
            model_runner=model_runner,
            context_manager=context_manager,
        )

    def _create_planner(
        self,
        *,
        project_root: Path,
        config: ProductionConfig,
        trace: TraceRecorder,
        identity: RunIdentity,
        user_goal: str,
        workspace_health: WorkspaceHealthReport | None,
        workspace_state_manager: WorkspaceStateManager,
    ) -> Planner:
        return create_or_resume_planner(
            workspace_root=project_root,
            session_id=config.resume_session,
            task_id=identity.task_id,
            user_goal=user_goal,
            trace=trace,
            workspace_health=workspace_health or workspace_state_manager.get_workspace_health(),
            fallback_session_id=identity.session_id,
            session_run_mode=config.session_run_mode,
        )

    @staticmethod
    def _wire_planner(
        *,
        planner: Planner,
        config: ProductionConfig,
        infra: _InfraComponents,
        policy_sandbox: _PolicySandboxComponents,
        execution_core: _ExecutionCoreComponents,
        tool_protocol: _ToolProtocolEngines,
        verification_review: _VerificationReviewPipelines,
        model_runner: ModelRunner | None = None,
    ) -> None:
        planner.project_index = infra.project_index
        planner.memory_pipeline = infra.memory_pipeline
        if planner.state is not None:
            permission_profile = policy_sandbox.policy_engine.config.permission_profile
            if permission_profile is None:
                raise AgentGraphInitializationError(
                    "PolicyEngine permission profile is not initialized.",
                    code="permission_profile_unavailable",
                )
            permission_summary = permission_profile.summary().to_dict()
            planner.record_sandbox_capability(
                {
                    "mode": permission_summary["profile"],
                    "permission": permission_summary,
                    "enforcement_status": policy_sandbox.sandbox_manager.capability_summary()[
                        "backend_status"
                    ],
                }
            )
        execution_core.command_executor.planner = planner
        execution_core.mutation_manager.planner = planner
        verification_review.verification_runner.planner = planner
        execution_core.edit_executor.planner = planner
        verification_review.review_pipeline.planner = planner
        planner.review_pipeline = verification_review.review_pipeline
        tool_protocol.tool_executor.planner = planner
        # Inject Semantic Planner producer bundle so Planner.start_task/replan/
        # record_failure_analysis go through model-driven producers with rule
        # fallback. model_runner comes from _build_model_context; when None
        # (test/CI), producers auto-fallback to rules.
        from singularity.planner.semantic_producers import PlannerProducerBundle

        bundle = PlannerProducerBundle.with_rule_fallback(
            model_runner=model_runner,
            rule_builder=planner.contract_builder,
            rule_planner=planner.semantic_planner,
            rule_replanner=planner.replanner,
            trace=planner.trace,
        )
        planner.attach_producers(bundle)

    @staticmethod
    def _prime_planner_context(
        *,
        user_goal: str,
        planner: Planner,
        recovery_gate_decision: RecoveryGateDecision | None,
        project_index: ProjectIndex,
        memory_pipeline: MemoryLearningPipeline,
        context_manager: ContextManager,
    ) -> None:
        index_observation = project_index.observation_for_goal(user_goal)
        context_manager.add_project_index(index_observation)
        planner.record_project_index_observation(index_observation)
        memory_block = memory_pipeline.context_block(
            goal=user_goal,
            max_items=6,
            token_budget=512,
        )
        if memory_block.items:
            context_manager.add_memory_context_block(memory_block)

    @staticmethod
    def _evaluation_harness_factory(
        *,
        project_root: Path,
        trace: TraceRecorder,
        infra: _InfraComponents,
        execution_core: _ExecutionCoreComponents,
        tool_protocol: _ToolProtocolEngines,
        verification_review: _VerificationReviewPipelines,
        planner: Planner,
    ) -> Callable[[], EvaluationHarness]:
        def build_evaluation_harness() -> EvaluationHarness:
            return EvaluationHarness(
                project_root=project_root,
                trace_recorder=trace,
                verification_runner=verification_review.verification_runner,
                memory_pipeline=infra.memory_pipeline,
                planner=planner,
                tool_executor=tool_protocol.tool_executor,
                command_executor=execution_core.command_executor,
                mutation_manager=execution_core.mutation_manager,
            )

        return build_evaluation_harness

    @staticmethod
    def _mark_first_pending_failed(
        components: dict[ComponentName, ComponentState],
    ) -> None:
        for component, state in list(components.items()):
            if state == ComponentState.PENDING:
                components[component] = ComponentState.FAILED
                break


def _workspace_state_context_hook(workspace_state_manager: WorkspaceStateManager):
    def hook(context, *, batch, tool_call_id: str | None) -> None:
        _ = batch, tool_call_id
        workspace_state_manager.record_external_changes()
        context.add_workspace_state(workspace_state_manager.get_workspace_health().to_observation())

    return hook
