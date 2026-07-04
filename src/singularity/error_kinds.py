from __future__ import annotations

from enum import StrEnum


class ToolCallFailureKind(StrEnum):
    missing_tool_call_id = "missing_tool_call_id"
    duplicate_tool_call_id = "duplicate_tool_call_id"
    unknown_tool = "unknown_tool"
    disallowed_tool = "disallowed_tool"
    invalid_json = "invalid_json"
    arguments_not_object = "arguments_not_object"
    schema_mismatch = "schema_mismatch"
    protocol_violation = "protocol_violation"
    policy_denied = "policy_denied"
    approval_required = "approval_required"
    approval_denied = "approval_denied"
    sandbox_required = "sandbox_required"
    tool_executor_failed = "tool_executor_failed"
    result_binding_failed = "result_binding_failed"
    replay_detected = "replay_detected"
    conflicting_replay = "conflicting_replay"
    context_append_failed = "context_append_failed"
