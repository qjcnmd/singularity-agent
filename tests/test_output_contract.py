"""Tests for the Model Output Contract Layer (``singularity.model.output``).

Covers:
- OutputParser: pure JSON, markdown-fenced JSON, non-JSON, normalization
- OutputContract: missing required fields, enum violations, type errors
- OutputRepairer: safe repairs (whitespace, case, int→float), dangerous field refusal
- OutputGuardrail: affected_files path escape, verification command missing
- Integration: semantic producer fallback behavior unchanged
- Integration: failure analysis invalid output → blocked (no repair execution)
"""

from __future__ import annotations

import json
from typing import Any
from uuid import uuid4

import pytest

from singularity.model.output import (
    FAILURE_ANALYSIS_OUTPUT_CONTRACT,
    PLANNER_DECISION_OUTPUT_CONTRACT,
    SEMANTIC_PLAN_OUTPUT_CONTRACT,
    TASK_CONTRACT_OUTPUT_CONTRACT,
    ERROR_ENUM_VIOLATION,
    ERROR_INVALID_JSON,
    ERROR_MISSING_REQUIRED_FIELD,
    ERROR_NOT_OBJECT,
    ERROR_UNAUTHORIZED_REFERENCE,
    ERROR_UNSAFE_AUTO_REPAIR,
    ERROR_WRONG_TYPE,
    FieldSchema,
    OutputContract,
    OutputGuardrail,
    OutputParseError,
    OutputParseResult,
    OutputParser,
    OutputRepairer,
)
from singularity.model.models import (
    ModelMessage,
    ModelPurpose,
    ModelTurnRequest,
    ModelTurnResult,
    ModelTurnStatus,
)
from singularity.planner.contract import TaskContractBuilder
from singularity.planner.replanner import Replanner
from singularity.planner.semantic import SemanticPlanner
from singularity.planner.semantic_producers import (
    PlannerDecisionProducer,
    SemanticPlanProducer,
    TaskContractProducer,
)

# ---------------------------------------------------------------------------
# Fakes
# ---------------------------------------------------------------------------


class FakeModelRunner:
    """Duck-typed fake that returns scripted JSON per ModelPurpose."""

    def __init__(self, responses: dict[Any, Any]) -> None:
        self.responses = responses
        self.requests: list[ModelTurnRequest] = []

    def run_turn(self, request: ModelTurnRequest) -> ModelTurnResult:
        self.requests.append(request)
        value = self.responses.get(request.purpose)
        if isinstance(value, Exception):
            raise value
        if value is None:
            return ModelTurnResult(
                request_id=request.request_id,
                response_id="resp_fake",
                status=ModelTurnStatus.FAILED,
            )
        return ModelTurnResult(
            request_id=request.request_id,
            response_id="resp_fake",
            status=ModelTurnStatus.SUCCESS,
            assistant_message=ModelMessage.assistant_text(value),
        )


class FakeTrace:
    """Minimal trace recorder."""

    def __init__(self) -> None:
        self.events: list[tuple[str, dict[str, Any]]] = []

    def emit(
        self,
        event: str,
        *,
        component: str = "",
        summary: str = "",
        payload: dict[str, Any] | None = None,
        ids: dict[str, Any] | None = None,
        severity: str = "info",
    ) -> None:
        self.events.append(
            (
                event,
                {
                    "component": component,
                    "summary": summary,
                    "payload": payload or {},
                    "ids": ids or {},
                    "severity": severity,
                },
            )
        )

    def record(self, event: str, payload: dict[str, Any] | None = None) -> None:
        self.events.append((event, payload or {}))

    def has_event(self, substring: str) -> bool:
        return any(substring in event for event, _ in self.events)


# ---------------------------------------------------------------------------
# Test data
# ---------------------------------------------------------------------------

PURE_JSON = json.dumps({"root_cause": "test", "failure_category": "tool_error", "confidence": 0.8, "needs_user_input": False, "evidence_refs": ["ref1"], "repair_strategy": "fix", "next_actions": ["do_x"]})

MARKDOWN_FENCED_JSON = '```json\n{"root_cause": "test", "failure_category": "tool_error", "confidence": 0.8, "needs_user_input": false, "evidence_refs": ["ref1"], "repair_strategy": "fix", "next_actions": ["do_x"]}\n```'

NON_JSON_TEXT = "This is just plain text, no JSON here at all."

JSON_ARRAY = json.dumps([1, 2, 3])

MISSING_REQUIRED = json.dumps({"confidence": 0.5, "needs_user_input": True})  # missing root_cause, etc.

ENUM_INVALID = json.dumps({"root_cause": "test", "failure_category": "INVALID!!!", "confidence": 0.8, "needs_user_input": False, "evidence_refs": ["ref1"], "repair_strategy": "fix", "next_actions": ["do_x"]})

WRONG_TYPE = json.dumps({"root_cause": "test", "failure_category": "tool_error", "confidence": "high", "needs_user_input": False, "evidence_refs": ["ref1"], "repair_strategy": "fix", "next_actions": ["do_x"]})

# ---------------------------------------------------------------------------
# 1. OutputParser — pure JSON success
# ---------------------------------------------------------------------------


class TestOutputParserPureJson:
    def test_pure_json_ok(self):
        parser = OutputParser()
        result = parser.parse(PURE_JSON)
        assert result.ok is True
        assert result.parsed == json.loads(PURE_JSON)
        assert result.normalization_reason is None

    def test_pure_json_no_errors(self):
        parser = OutputParser()
        result = parser.parse(PURE_JSON)
        assert result.errors == []


# ---------------------------------------------------------------------------
# 2. OutputParser — markdown fenced JSON + normalization
# ---------------------------------------------------------------------------


class TestOutputParserMarkdownFenced:
    def test_markdown_fence_ok(self):
        parser = OutputParser()
        result = parser.parse(MARKDOWN_FENCED_JSON)
        assert result.ok is True
        assert result.parsed == json.loads(PURE_JSON)

    def test_markdown_fence_normalization_reason(self):
        parser = OutputParser()
        result = parser.parse(MARKDOWN_FENCED_JSON)
        assert result.normalization_reason == "markdown_fence_stripped"


# ---------------------------------------------------------------------------
# 3. OutputParser — non-JSON failure
# ---------------------------------------------------------------------------


class TestOutputParserNonJson:
    def test_plain_text_fails(self):
        parser = OutputParser()
        result = parser.parse(NON_JSON_TEXT)
        assert result.ok is False
        assert result.parsed is None

    def test_plain_text_invalid_json_error(self):
        parser = OutputParser()
        result = parser.parse(NON_JSON_TEXT)
        assert len(result.errors) == 1
        assert result.errors[0].code == ERROR_INVALID_JSON

    def test_array_not_object(self):
        parser = OutputParser()
        result = parser.parse(JSON_ARRAY)
        assert result.ok is False
        assert result.errors[0].code == ERROR_NOT_OBJECT

    def test_empty_string(self):
        parser = OutputParser()
        result = parser.parse("")
        assert result.ok is False

    def test_non_string_input(self):
        parser = OutputParser()
        result = parser.parse(123)  # type: ignore[arg-type]
        assert result.ok is False
        assert result.errors[0].code == ERROR_INVALID_JSON


# ---------------------------------------------------------------------------
# 4. OutputContract — missing required field
# ---------------------------------------------------------------------------


class TestOutputContractMissingRequired:
    def test_missing_required_detected(self):
        payload = json.loads(MISSING_REQUIRED)
        errors = FAILURE_ANALYSIS_OUTPUT_CONTRACT.validate(payload)
        missing_fields = {e.field for e in errors if e.code == ERROR_MISSING_REQUIRED_FIELD}
        assert "root_cause" in missing_fields
        assert "evidence_refs" in missing_fields
        assert "repair_strategy" in missing_fields

    def test_all_required_present_passes(self):
        payload = json.loads(PURE_JSON)
        errors = FAILURE_ANALYSIS_OUTPUT_CONTRACT.validate(payload)
        assert errors == []


# ---------------------------------------------------------------------------
# 5. OutputContract — enum violation
# ---------------------------------------------------------------------------


class TestOutputContractEnumViolation:
    def test_invalid_enum_detected(self):
        contract = OutputContract(
            fields=[
                FieldSchema("status", type_=str, required=True, enum_values=["ok", "fail", "retry"]),
            ]
        )
        payload = {"status": "INVALID!!!"}
        errors = contract.validate(payload)
        enum_errors = [e for e in errors if e.code == ERROR_ENUM_VIOLATION]
        assert len(enum_errors) >= 1
        assert any(e.field == "status" for e in enum_errors)


# ---------------------------------------------------------------------------
# 6. OutputContract — wrong type
# ---------------------------------------------------------------------------


class TestOutputContractWrongType:
    def test_wrong_type_detected(self):
        payload = json.loads(WRONG_TYPE)
        errors = FAILURE_ANALYSIS_OUTPUT_CONTRACT.validate(payload)
        type_errors = [e for e in errors if e.code == ERROR_WRONG_TYPE]
        assert any(e.field == "confidence" for e in type_errors)


# ---------------------------------------------------------------------------
# 7. OutputRepairer — safe repairs
# ---------------------------------------------------------------------------


class TestOutputRepairerSafe:
    def test_repair_whitespace_enum(self):
        """Enum value with only whitespace difference is auto-stripped."""
        contract = OutputContract(
            fields=[
                FieldSchema("status", type_=str, required=True, enum_values=["ok", "fail", "retry"]),
            ]
        )
        repairer = OutputRepairer()
        # "  OK  " normalizes to "ok" which IS in the enum → no validation error
        # The repairer's final strip pass removes whitespace
        payload = {"status": "  OK  "}
        errors = contract.validate(payload)
        # No enum error since validator normalizes before checking
        assert errors == []
        result = repairer.repair(payload, errors, contract=contract)
        assert result.ok is True
        assert result.parsed == {"status": "OK"}  # stripped but not lowercased (no error triggered case fix)

    def test_repair_enum_case_normalization(self):
        """When enum validation fails due to case, repairer normalizes case."""
        contract = OutputContract(
            fields=[
                FieldSchema("status", type_=str, required=True, enum_values=["ok", "fail", "retry"]),
            ]
        )
        repairer = OutputRepairer()
        # "OKAY" normalizes to "okay" which is NOT in the enum → validation error
        payload = {"status": "OKAY"}
        errors = contract.validate(payload)
        assert len(errors) == 1
        assert errors[0].code == ERROR_ENUM_VIOLATION
        # Repair cannot map "okay" to any allowed value → repair fails
        result = repairer.repair(payload, errors, contract=contract)
        assert result.ok is False

    def test_int_to_float_repair(self):
        contract = OutputContract(
            fields=[
                FieldSchema("confidence", type_=(int, float), required=True),
            ]
        )
        repairer = OutputRepairer()
        payload = {"confidence": 1}  # int → float OK
        errors = contract.validate(payload)
        # int is accepted by type check (float tolerates int), so no errors
        assert errors == []

    def test_str_to_int_repair(self):
        contract = OutputContract(
            fields=[
                FieldSchema("count", type_=int, required=True),
            ]
        )
        repairer = OutputRepairer()
        payload = {"count": "42"}
        errors = contract.validate(payload)
        result = repairer.repair(payload, errors, contract=contract)
        assert result.ok is True
        assert result.parsed == {"count": 42}


# ---------------------------------------------------------------------------
# 8. OutputRepairer — dangerous fields NOT repaired
# ---------------------------------------------------------------------------


class TestOutputRepairerDangerous:
    def test_affected_files_not_repaired(self):
        """affected_files is dangerous → repair must fail-closed."""
        contract = OutputContract(
            fields=[
                FieldSchema("affected_files", type_=list, required=True, dangerous=True, allow_repair=False),
            ]
        )
        repairer = OutputRepairer()
        payload: dict[str, Any] = {}
        errors = contract.validate(payload)
        result = repairer.repair(payload, errors, contract=contract)
        assert result.ok is False
        assert any(e.code == ERROR_UNSAFE_AUTO_REPAIR for e in result.errors)

    def test_missing_required_cannot_be_invented(self):
        contract = OutputContract(
            fields=[
                FieldSchema("root_cause", type_=str, required=True),
            ]
        )
        repairer = OutputRepairer()
        payload: dict[str, Any] = {}
        errors = contract.validate(payload)
        result = repairer.repair(payload, errors, contract=contract)
        assert result.ok is False
        assert any(e.code == ERROR_UNSAFE_AUTO_REPAIR for e in result.errors)


# ---------------------------------------------------------------------------
# 9. OutputGuardrail — affected_files path escape
# ---------------------------------------------------------------------------


class TestOutputGuardrailAffectedFiles:
    def test_path_escape_blocked(self):
        guardrail = OutputGuardrail()
        contract = OutputContract(
            fields=[
                FieldSchema("affected_files", type_=list, required=False, dangerous=True),
            ]
        )
        errors = guardrail.check(
            {"affected_files": ["/etc/passwd"]},
            contract=contract,
            context={"workspace_root": "/home/user/project"},
        )
        assert len(errors) >= 1
        assert any(e.code == ERROR_UNAUTHORIZED_REFERENCE for e in errors)

    def test_allowed_path_passes(self):
        guardrail = OutputGuardrail()
        contract = OutputContract(
            fields=[
                FieldSchema("affected_files", type_=list, required=False, dangerous=True),
            ]
        )
        errors = guardrail.check(
            {"affected_files": ["src/main.py"]},
            contract=contract,
            context={
                "workspace_root": "/home/user/project",
                "allowed_target_files": ["src/main.py"],
            },
        )
        # Path is relative and within workspace
        assert errors == []


# ---------------------------------------------------------------------------
# 10. OutputGuardrail — verification command missing → no guess
# ---------------------------------------------------------------------------


class TestOutputGuardrailVerificationMissing:
    def test_empty_verification_plan_blocked(self):
        guardrail = OutputGuardrail()
        contract = OutputContract(
            fields=[
                FieldSchema("verification_plan", type_=list, required=False, dangerous=True, allow_repair=False),
            ]
        )
        errors = guardrail.check(
            {"verification_plan": []},
            contract=contract,
        )
        assert len(errors) >= 1
        assert any(e.code == ERROR_UNSAFE_AUTO_REPAIR for e in errors)

    def test_non_empty_verification_plan_passes(self):
        guardrail = OutputGuardrail()
        contract = OutputContract(
            fields=[
                FieldSchema("verification_plan", type_=list, required=False, dangerous=True, allow_repair=False),
            ]
        )
        errors = guardrail.check(
            {"verification_plan": ["run pytest"]},
            contract=contract,
        )
        assert errors == []


# ---------------------------------------------------------------------------
# 11. Semantic producer fallback behavior unchanged
# ---------------------------------------------------------------------------


VALID_TASK_CONTRACT_JSON = json.dumps({
    "user_goal": "Create hello.py",
    "acceptance_criteria": [
        {"criterion_id": "ac1", "description": "File exists", "evidence": ["file_check"], "required": True}
    ],
    "deliverables": [],
    "verification_requirements": [],
    "constraints": [],
})

VALID_SEMANTIC_PLAN_JSON = json.dumps({
    "rolling_plan": {
        "plan_id": "plan_1",
        "user_goal": "Create hello.py",
        "current_step_id": "s1",
        "version": 1,
        "steps": [
            {
                "step_id": "s1",
                "title": "Write file",
                "kind": "InspectWorkspace",
                "acceptance_criterion_id": "ac1",
                "dependencies": [],
                "allowed_capabilities": [],
                "expected_evidence": [],
                "fallback_steps": [],
                "status": "pending",
            }
        ],
    },
    "risk_points": [],
    "verification_strategies": [],
    "repair_policy": None,
})

VALID_DECISION_JSON = json.dumps({
    "decision": "continue",
    "reason": "Everything looks good.",
    "next_action": "InspectWorkspace",
    "risk_points_triggered": [],
    "verification_strategy_selected": None,
})

INVALID_JSON_GARBAGE = "not json at all {{{"


class TestSemanticProducerFallbackUnchanged:
    """Verify all three producers fall back to rules on parse failure — behavior unchanged."""

    def test_task_contract_producer_fallback_on_garbage_json(self):
        runner = FakeModelRunner({ModelPurpose.TASK_CONTRACT_EXTRACTION: INVALID_JSON_GARBAGE})
        builder = TaskContractBuilder()
        trace = FakeTrace()
        producer = TaskContractProducer(model_runner=runner, rule_builder=builder, trace=trace)
        contract = producer.produce("Create hello.py", context_payload={"task_id": "t1"})
        # Falls back to rules
        assert contract.source == "rules"
        assert trace.has_event("fallback")

    def test_semantic_plan_producer_fallback_on_garbage_json(self):
        runner = FakeModelRunner({ModelPurpose.SEMANTIC_PLANNING: INVALID_JSON_GARBAGE})
        planner = SemanticPlanner()
        trace = FakeTrace()
        producer = SemanticPlanProducer(model_runner=runner, rule_planner=planner, trace=trace)
        from singularity.planner.contract import TaskContract
        contract = TaskContract(user_goal="test", acceptance_criteria=[])
        plan = producer.produce_initial(contract, context_payload={"task_id": "t1"})
        # Falls back to rules
        assert plan.producer_source == "rules_fallback"
        assert trace.has_event("fallback")

    def test_planner_decision_producer_fallback_on_garbage_json(self):
        runner = FakeModelRunner({ModelPurpose.PLANNER_DECISION: INVALID_JSON_GARBAGE})
        replanner = Replanner()
        trace = FakeTrace()
        producer = PlannerDecisionProducer(model_runner=runner, rule_replanner=replanner, trace=trace)
        signal = {"failure_type": "verification_failed"}
        decision = producer.produce(signal, context_payload={"task_id": "t1"}, risk_points=[], verification_strategies=[], repair_policy=None)
        # Falls back to rules
        assert decision.producer_source == "rules_fallback"
        assert trace.has_event("fallback")

    def test_task_contract_producer_succeeds_with_valid_json(self):
        runner = FakeModelRunner({ModelPurpose.TASK_CONTRACT_EXTRACTION: VALID_TASK_CONTRACT_JSON})
        builder = TaskContractBuilder()
        trace = FakeTrace()
        producer = TaskContractProducer(model_runner=runner, rule_builder=builder, trace=trace)
        contract = producer.produce("Create hello.py", context_payload={"task_id": "t1"})
        assert contract.source == "model"
        assert trace.has_event("model_ok")

    def test_semantic_plan_producer_succeeds_with_valid_json(self):
        runner = FakeModelRunner({ModelPurpose.SEMANTIC_PLANNING: VALID_SEMANTIC_PLAN_JSON})
        planner = SemanticPlanner()
        trace = FakeTrace()
        producer = SemanticPlanProducer(model_runner=runner, rule_planner=planner, trace=trace)
        from singularity.planner.contract import TaskContract
        contract = TaskContract(user_goal="test", acceptance_criteria=[])
        plan = producer.produce_initial(contract, context_payload={"task_id": "t1"})
        assert plan.producer_source == "model"
        assert trace.has_event("model_ok")

    def test_planner_decision_producer_succeeds_with_valid_json(self):
        runner = FakeModelRunner({ModelPurpose.PLANNER_DECISION: VALID_DECISION_JSON})
        replanner = Replanner()
        trace = FakeTrace()
        producer = PlannerDecisionProducer(model_runner=runner, rule_replanner=replanner, trace=trace)
        signal = {"failure_type": "verification_failed"}
        decision = producer.produce(signal, context_payload={"task_id": "t1"}, risk_points=[], verification_strategies=[], repair_policy=None)
        assert decision.producer_source == "model"
        assert trace.has_event("model_ok")


# ---------------------------------------------------------------------------
# 12. Failure analysis invalid output → blocked (no repair execution)
# ---------------------------------------------------------------------------


class TestFailureAnalysisInvalidOutputBlocked:
    """FailureAnalyzer returns blocked() when model output is invalid — no repair plan execution."""

    def test_blocked_on_invalid_json(self):
        from singularity.failure_analysis.analyzer import FailureAnalyzer
        from singularity.failure_analysis.request import FailureAnalysisRequest

        runner = FakeModelRunner({ModelPurpose.FAILURE_ANALYSIS: INVALID_JSON_GARBAGE})
        trace = FakeTrace()
        analyzer = FailureAnalyzer(model_runner=runner, trace=trace)
        request = FailureAnalysisRequest(
            request_id="req_1",
            run_id="run_1",
            session_id="sess_1",
            task_id="task_1",
            phase_id="failure_analysis",
            workspace_root="/home/user/project",
            failure_source="verification",
            failure_summary="test failure",
            failure_sources=[{"failure_type": "verification_failed"}],
            evidence_refs=["ref1"],
            context_references=[],
            verification_log_refs=[],
            changed_files=["src/main.py"],
        )
        result = analyzer.analyze(request)
        # Must be blocked
        assert result.needs_user_input is True
        assert result.confidence == 0.0
        assert "invalid_json" in result.failure_category or "blocked" in (result.blocked_reason or "")

    def test_blocked_does_not_produce_repair_actions(self):
        """Blocked analysis has repair_strategy='blocked' → no repair execution."""
        from singularity.failure_analysis.analyzer import FailureAnalyzer
        from singularity.failure_analysis.request import FailureAnalysisRequest

        runner = FakeModelRunner({ModelPurpose.FAILURE_ANALYSIS: INVALID_JSON_GARBAGE})
        analyzer = FailureAnalyzer(model_runner=runner)
        request = FailureAnalysisRequest(
            request_id="req_1",
            run_id="run_1",
            session_id="sess_1",
            task_id="task_1",
            phase_id="failure_analysis",
            workspace_root="/home/user/project",
            failure_source="verification",
            failure_summary="test failure",
            failure_sources=[{"failure_type": "verification_failed"}],
            evidence_refs=["ref1"],
            context_references=[],
            verification_log_refs=[],
            changed_files=["src/main.py"],
        )
        result = analyzer.analyze(request)
        # repair_strategy is "blocked", not executable
        assert result.repair_strategy == "blocked"


# ---------------------------------------------------------------------------
# 13. OutputParseError serialization
# ---------------------------------------------------------------------------


class TestOutputParseErrorSerialization:
    def test_to_dict_round_trip(self):
        error = OutputParseError(
            code=ERROR_MISSING_REQUIRED_FIELD,
            message="required field 'root_cause' is missing",
            field="root_cause",
        )
        d = error.to_dict()
        assert d["code"] == ERROR_MISSING_REQUIRED_FIELD
        assert d["message"] == "required field 'root_cause' is missing"
        assert d["field"] == "root_cause"

    def test_to_dict_minimal(self):
        error = OutputParseError(code=ERROR_INVALID_JSON, message="bad json")
        d = error.to_dict()
        assert d["code"] == ERROR_INVALID_JSON
        assert "field" not in d
        assert "raw_value_repr" not in d


# ---------------------------------------------------------------------------
# 14. OutputParser — regex brace extraction (tier 3 fallback)
# ---------------------------------------------------------------------------


class TestOutputParserRegexBraceExtraction:
    def test_regex_extraction_with_prefix_text(self):
        parser = OutputParser()
        text = 'Some preamble text...\n{"key": "value"}\nSome trailing text.'
        result = parser.parse(text)
        assert result.ok is True
        assert result.parsed == {"key": "value"}
        assert result.normalization_reason == "regex_brace_extraction"

    def test_regex_extraction_fails_when_no_braces(self):
        parser = OutputParser()
        result = parser.parse("no braces at all")
        assert result.ok is False


# ---------------------------------------------------------------------------
# 15. Predefined contracts — structural checks
# ---------------------------------------------------------------------------


class TestPredefinedContracts:
    def test_failure_analysis_contract_has_dangerous_fields(self):
        contract = FAILURE_ANALYSIS_OUTPUT_CONTRACT
        dangerous = [f.name for f in contract._fields.values() if f.dangerous]
        assert "affected_files" in dangerous
        assert "verification_plan" in dangerous

    def test_task_contract_contract_has_dangerous_fields(self):
        contract = TASK_CONTRACT_OUTPUT_CONTRACT
        dangerous = [f.name for f in contract._fields.values() if f.dangerous]
        assert "verification_requirements" in dangerous

    def test_semantic_plan_contract_has_dangerous_fields(self):
        contract = SEMANTIC_PLAN_OUTPUT_CONTRACT
        dangerous = [f.name for f in contract._fields.values() if f.dangerous]
        assert "verification_strategies" in dangerous

    def test_planner_decision_contract_no_dangerous(self):
        contract = PLANNER_DECISION_OUTPUT_CONTRACT
        dangerous = [f.name for f in contract._fields.values() if f.dangerous]
        assert dangerous == []