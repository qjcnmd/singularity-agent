from __future__ import annotations

from typing import Any

from pydantic import BaseModel, ConfigDict, Field, model_validator

from miniharness.policy import Capability, OperationKind, ResourceRef
from miniharness.command import (
    CommandPurpose,
    CommandRequest,
    CommandRuntime,
    FilesystemMode,
    NetworkMode,
    ResourceLimits,
)
from miniharness.tools.models import (
    PermissionLevel,
    ToolExecutionBackendKind,
    ToolExecutionFailure,
    ToolSensitivityLevel,
    ToolSideEffectKind,
    ToolSpec,
)


class RunCommandInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    argv: list[str] | None = Field(None, description="Structured argv command.")
    shell: str | None = Field(None, description="Shell command string; high risk.")
    cwd: str = Field(".", description="Workspace-relative working directory.")
    purpose: str = Field("UNKNOWN", description="CommandPurpose enum name.")
    timeout_seconds: float | None = Field(None, gt=0)
    idle_timeout_seconds: float | None = Field(None, gt=0)
    env_request: dict[str, str] = Field(default_factory=dict)
    network_mode: str = Field("DISABLED")
    filesystem_mode: str = Field("READ_ONLY_WORKSPACE")
    resource_limits: dict[str, Any] = Field(default_factory=dict)
    expected_outputs: list[str] = Field(default_factory=list)
    risk_acceptance_reason: str | None = None

    @model_validator(mode="after")
    def _argv_or_shell(self) -> "RunCommandInput":
        if bool(self.argv) == bool(self.shell):
            raise ValueError("Exactly one of argv or shell is required.")
        if self.argv is not None and len(self.argv) == 0:
            raise ValueError("argv must not be empty.")
        return self


class ProcessIdInput(BaseModel):
    model_config = ConfigDict(extra="forbid")

    process_id: str


class EmptyInput(BaseModel):
    model_config = ConfigDict(extra="forbid")


class CommandToolHandlers:
    def __init__(self, runtime: CommandRuntime) -> None:
        self.runtime = runtime

    def run_command(self, args: RunCommandInput) -> dict[str, Any]:
        if _requires_verification_runtime(args):
            raise ToolExecutionFailure(
                "Verification-like commands must use VerificationRuntime tools.",
                code="verification_runtime_required",
                details={"suggested_tool": "run_verification"},
            )
        request = self._request(args)
        return self.runtime.run(request).to_observation()

    def start_process(self, args: RunCommandInput) -> dict[str, Any]:
        request = self._request(args)
        session = self.runtime.start_process(request)
        return {"process_session": session.to_dict()}

    def read_process_output(self, args: ProcessIdInput) -> dict[str, Any]:
        output = self.runtime.read_process_output(args.process_id)
        return {"process_output": output.to_dict()}

    def stop_process(self, args: ProcessIdInput) -> dict[str, Any]:
        stopped = self.runtime.stop_process(args.process_id)
        return {"process_stop": stopped.to_dict()}

    def list_processes(self, _args: EmptyInput) -> dict[str, Any]:
        return {
            "processes": [
                session.to_dict() for session in self.runtime.list_processes()
            ]
        }

    @staticmethod
    def _request(args: RunCommandInput) -> CommandRequest:
        limits_payload = dict(args.resource_limits)
        limits = ResourceLimits(**limits_payload) if limits_payload else ResourceLimits()
        return CommandRequest(
            argv=args.argv,
            shell=args.shell,
            cwd=args.cwd,
            purpose=CommandPurpose[args.purpose],
            timeout_seconds=args.timeout_seconds,
            idle_timeout_seconds=args.idle_timeout_seconds,
            env_request=args.env_request,
            network_mode=NetworkMode[args.network_mode],
            filesystem_mode=FilesystemMode[args.filesystem_mode],
            resource_limits=limits,
            expected_outputs=args.expected_outputs,
            risk_acceptance_reason=args.risk_acceptance_reason,
        )


def _requires_verification_runtime(args: RunCommandInput) -> bool:
    verification_purposes = {
        "PROJECT_VERIFICATION",
        "BUILD",
        "LINT",
        "TYPECHECK",
        "FORMAT_CHECK",
    }
    if args.purpose in verification_purposes:
        return True
    argv = [str(part).lower() for part in (args.argv or [])]
    if not argv:
        return False
    joined = " ".join(argv)
    program = argv[0].rsplit("\\", 1)[-1].rsplit("/", 1)[-1]
    if program.endswith((".exe", ".cmd", ".bat")):
        program = program.rsplit(".", 1)[0]
    if program in {"pytest", "tox", "nox", "jest", "vitest", "tsc", "eslint", "mypy", "pyright"}:
        return True
    if program in {"python", "python3", "py"} and len(argv) >= 3:
        return argv[1:3] in (
            ["-m", "pytest"],
            ["-m", "mypy"],
            ["-m", "ruff"],
            ["-m", "build"],
        )
    if program in {"npm", "pnpm", "yarn"}:
        return any(part in {"test", "lint", "build", "typecheck", "type-check"} for part in argv[1:3])
    if program == "cargo":
        return any(part in {"test", "build", "clippy", "check"} for part in argv[1:3])
    if program == "go":
        return any(part in {"test", "build"} for part in argv[1:3])
    if program in {"make", "just"}:
        return any(part in {"test", "lint", "build", "typecheck"} for part in argv[1:3])
    return " pytest" in joined or " npm test" in joined or " pnpm test" in joined


def _command_identifier(args: dict[str, Any]) -> str:
    if args.get("shell"):
        return str(args["shell"])
    if args.get("argv"):
        return " ".join(str(part) for part in args["argv"])
    return ""


def register_command_tools(registry: Any, runtime: CommandRuntime | None = None) -> None:
    command_runtime = runtime or CommandRuntime(registry.project_root)
    handlers = CommandToolHandlers(command_runtime)
    registry.register(
        ToolSpec(
            name="run_command",
            version="0.0.7",
            description="Run a structured command through CommandRuntime policy, limits, output collection, trace, and side-effect tracking.",
            input_model=RunCommandInput,
            handler=handlers.run_command,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.EXECUTE_COMMAND,
            resource_resolver=lambda args, _root: [
                ResourceRef("command", _command_identifier(args))
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            risk_tags=("command_runtime",),
            timeout_seconds=60.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_command_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="start_process",
            version="0.0.7",
            description="Start a long-running process session through CommandRuntime.",
            input_model=RunCommandInput,
            handler=handlers.start_process,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.START_LONG_PROCESS,),
            operation=OperationKind.START_LONG_PROCESS,
            resource_resolver=lambda args, _root: [
                ResourceRef("command", _command_identifier(args))
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            risk_tags=("command_runtime", "long_running"),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_command_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="read_process_output",
            version="0.0.7",
            description="Read buffered output from a CommandRuntime process session.",
            input_model=ProcessIdInput,
            handler=handlers.read_process_output,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.READ_FILE,
            resource_resolver=lambda args, _root: [
                ResourceRef("process", args.get("process_id") or "")
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            risk_tags=("command_runtime",),
            timeout_seconds=5.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=True,
            uses_command_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="stop_process",
            version="0.0.7",
            description="Stop a CommandRuntime process session and clean up its process tree.",
            input_model=ProcessIdInput,
            handler=handlers.stop_process,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.KILL_PROCESS,),
            operation=OperationKind.KILL_PROCESS,
            resource_resolver=lambda args, _root: [
                ResourceRef("process", args.get("process_id") or "")
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            risk_tags=("command_runtime",),
            timeout_seconds=10.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=False,
            uses_command_runtime=True,
        )
    )
    registry.register(
        ToolSpec(
            name="list_processes",
            version="0.0.7",
            description="List CommandRuntime process sessions.",
            input_model=EmptyInput,
            handler=handlers.list_processes,
            permission_level=PermissionLevel.SHELL,
            capabilities=(Capability.EXECUTE_COMMAND,),
            operation=OperationKind.LIST_DIRECTORY,
            resource_resolver=lambda _args, _root: [
                ResourceRef("process", "command_runtime_sessions")
            ],
            side_effects=ToolSideEffectKind.EXECUTE_COMMAND,
            sensitivity=ToolSensitivityLevel.WORKSPACE,
            execution_backend=ToolExecutionBackendKind.DELEGATED_COMMAND_RUNTIME,
            risk_tags=("command_runtime",),
            timeout_seconds=5.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=True,
            uses_command_runtime=True,
        )
    )
