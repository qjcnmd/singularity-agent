import json
from pathlib import Path

from miniharness.policy import (
    Capability,
    DecisionOutcome,
    OperationKind,
    PolicyAuditWriter,
    PolicyConfig,
    PolicyDecision,
    PolicyRequest,
    PolicySubject,
    ResourceRef,
    RuntimeName,
)


def test_policy_audit_writes_jsonl_and_redacts_secrets(tmp_path: Path) -> None:
    audit_path = tmp_path / "policy.jsonl"
    writer = PolicyAuditWriter(PolicyConfig(workspace_root=tmp_path, audit_log_path=audit_path))
    request = PolicyRequest(
        session_id="session",
        task_id="task",
        phase_id="phase",
        action_id="action",
        runtime=RuntimeName.COMMAND,
        operation=OperationKind.NETWORK_ACCESS,
        capability=Capability.NETWORK_ACCESS,
        subject=PolicySubject(subject_type="runtime", name="test"),
        resource=ResourceRef(resource_type="network", identifier="https://example.test"),
        reason="Authorization: Bearer secret-token OPENAI_API_KEY=sk-test",
        workspace_root=str(tmp_path),
        metadata={"token": "secret-token", "Authorization": "Bearer secret-token"},
    )
    decision = PolicyDecision(
        request_id=request.request_id,
        outcome=DecisionOutcome.REQUIRE_REVIEW,
        reason="network requires review",
    )

    writer.append(request=request, decision=decision)

    line = audit_path.read_text(encoding="utf-8").splitlines()[0]
    payload = json.loads(line)
    text = json.dumps(payload)
    assert payload["request_id"] == request.request_id
    assert payload["decision_id"] == decision.decision_id
    assert payload["outcome"] == "require_review"
    assert "secret-token" not in text
    assert "sk-test" not in text
    assert "[REDACTED]" in text
