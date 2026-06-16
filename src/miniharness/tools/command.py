from __future__ import annotations

from typing import Any

from pydantic import BaseModel, Field

from miniharness.command import (
    CommandPurpose,
    CommandRequest,
    CommandRuntime,
    FilesystemMode,
    NetworkMode,
    ResourceLimits,
)
from miniharness.tools.models import PermissionLevel, ToolSpec


class RunCommandInput(BaseModel):
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


class ProcessIdInput(BaseModel):
    process_id: str


class EmptyInput(BaseModel):
    pass


class CommandToolHandlers:
    def __init__(self, runtime: CommandRuntime) -> None:
        self.runtime = runtime

    def run_command(self, args: RunCommandInput) -> dict[str, Any]:
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
            risk_tags=("command_runtime",),
            timeout_seconds=5.0,
            max_output_chars=12000,
            cacheable=False,
            idempotent=True,
            uses_command_runtime=True,
        )
    )
