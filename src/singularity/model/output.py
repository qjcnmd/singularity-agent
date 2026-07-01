"""Model Output Contract Layer.

Unified parsing, validation, repair, and fallback for model text/JSON outputs.
Replaces the duplicated ``_json_payload()`` functions previously scattered across
``failure_analysis/result.py``, ``planner/semantic_producers.py``, and
``planner/final_reviewer.py``.

Three-stage pipeline
--------------------

1. **OutputParser.parse()** — deterministic parse/normalize
   - ``json.loads()`` → strip markdown fences → regex ``{...}`` fallback
   - Records ``normalization_reason`` when a non-trivial path is taken
   - Trace: ``output.parse.started`` / ``output.parse.succeeded`` / ``output.parse.failed`` / ``output.normalized``

2. **OutputContract.validate()** — schema validation
   - Checks required fields, types, enum values, custom validators
   - Returns structured ``list[OutputParseError]``

3. **OutputRepairer.repair()** / **OutputGuardrail.check()** — safe repair / fallback
   - Only unambiguous, non-dangerous fixes (whitespace, case, int→float, defaults)
   - Fail-closed on commands, paths, permissions, workspace mutations
   - Trace: ``output.repair.requested`` / ``output.repair.succeeded`` / ``output.repair.failed`` / ``output.fallback.used``

Error codes
-----------
``invalid_json``, ``not_object``, ``missing_required_field``, ``wrong_type``,
``enum_violation``, ``unauthorized_reference``, ``unsafe_auto_repair``,
``semantic_inconsistency``, ``provider_refusal``, ``length_truncated``

Existing object boundaries (``TaskContract``, ``SemanticPlan``, ``PlannerDecision``,
``FailureAnalysisResult``) are preserved — this layer sits between raw model text
and the existing ``from_model_payload()`` / ``from_dict()`` constructors.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable
from dataclasses import dataclass, field
from typing import Any

# ---------------------------------------------------------------------------
# Error codes
# ---------------------------------------------------------------------------

ERROR_INVALID_JSON = "invalid_json"
ERROR_NOT_OBJECT = "not_object"
ERROR_MISSING_REQUIRED_FIELD = "missing_required_field"
ERROR_WRONG_TYPE = "wrong_type"
ERROR_ENUM_VIOLATION = "enum_violation"
ERROR_UNAUTHORIZED_REFERENCE = "unauthorized_reference"
ERROR_UNSAFE_AUTO_REPAIR = "unsafe_auto_repair"
ERROR_SEMANTIC_INCONSISTENCY = "semantic_inconsistency"
ERROR_PROVIDER_REFUSAL = "provider_refusal"
ERROR_LENGTH_TRUNCATED = "length_truncated"

# ---------------------------------------------------------------------------
# OutputParseError
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class OutputParseError:
    """Structured error from model output parsing / validation / repair."""

    code: str
    message: str
    field: str | None = None
    raw_value_repr: str | None = None
    normalization_reason: str | None = None

    def to_dict(self) -> dict[str, Any]:
        d: dict[str, Any] = {"code": self.code, "message": self.message}
        if self.field is not None:
            d["field"] = self.field
        if self.raw_value_repr is not None:
            d["raw_value_repr"] = self.raw_value_repr
        if self.normalization_reason is not None:
            d["normalization_reason"] = self.normalization_reason
        return d


# ---------------------------------------------------------------------------
# OutputParseResult
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class OutputParseResult:
    """Result from any stage of the output contract pipeline."""

    ok: bool
    parsed: dict[str, Any] | None = None
    errors: list[OutputParseError] = field(default_factory=list)
    normalization_reason: str | None = None


# ---------------------------------------------------------------------------
# FieldSchema
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class FieldSchema:
    """Schema definition for a single field in an ``OutputContract``."""

    name: str
    type_: type | tuple[type, ...] = str  # str, int, float, bool, list, dict, or tuple of types
    required: bool = False
    enum_values: list[str] | None = None
    allow_repair: bool = True  # if False, repair will not touch this field
    dangerous: bool = False  # if True, auto-repair is blocked; fail-closed


# ---------------------------------------------------------------------------
# OutputParser
# ---------------------------------------------------------------------------

_MARKDOWN_FENCE_RE = re.compile(
    r"```(?:json)?\s*(.*?)\s*```", flags=re.DOTALL | re.IGNORECASE
)
_JSON_OBJECT_RE = re.compile(r"\{.*\}", flags=re.DOTALL)


class OutputParser:
    """Deterministic JSON parse with markdown-fence and regex fallback.

    Three-tier extraction:

    1. ``json.loads(text)`` — pure JSON
    2. Strip markdown `` ```json ... ``` `` fences and retry ``json.loads()``
    3. Regex ``{...}`` extraction as last resort

    Records ``normalization_reason`` when tier 2 or 3 is used.
    """

    def __init__(self, *, max_length: int = 100_000) -> None:
        self.max_length = max_length

    # ------------------------------------------------------------------
    # public API
    # ------------------------------------------------------------------

    def parse(self, text: str) -> OutputParseResult:
        """Parse raw model text into a JSON dict.

        Returns:
            ``OutputParseResult`` with ``ok=True`` and ``parsed`` set on success,
            or ``ok=False`` with ``errors`` on failure.
        """
        if not isinstance(text, str):
            return OutputParseResult(
                ok=False,
                errors=[
                    OutputParseError(
                        code=ERROR_INVALID_JSON,
                        message="model output is not a string",
                    )
                ],
            )
        text = text.strip()
        if len(text) > self.max_length:
            text = text[: self.max_length]

        # Tier 1: direct json.loads
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            pass
        else:
            return self._result_from_value(value, normalization_reason=None)

        # Tier 2: strip markdown fences
        fence_match = _MARKDOWN_FENCE_RE.search(text)
        if fence_match:
            inner = fence_match.group(1).strip()
            try:
                value = json.loads(inner)
            except json.JSONDecodeError:
                pass
            else:
                return self._result_from_value(
                    value, normalization_reason="markdown_fence_stripped"
                )

        # Tier 3: regex { ... } extraction
        brace_match = _JSON_OBJECT_RE.search(text)
        if brace_match:
            try:
                value = json.loads(brace_match.group(0))
            except json.JSONDecodeError:
                pass
            else:
                return self._result_from_value(
                    value, normalization_reason="regex_brace_extraction"
                )

        # Complete failure
        return OutputParseResult(
            ok=False,
            errors=[
                OutputParseError(
                    code=ERROR_INVALID_JSON,
                    message="model response did not contain a parseable JSON object",
                )
            ],
        )

    # ------------------------------------------------------------------
    # internal helpers
    # ------------------------------------------------------------------

    def _result_from_value(
        self, value: Any, *, normalization_reason: str | None
    ) -> OutputParseResult:
        if not isinstance(value, dict):
            return OutputParseResult(
                ok=False,
                errors=[
                    OutputParseError(
                        code=ERROR_NOT_OBJECT,
                        message="model response JSON was not an object",
                    )
                ],
            )
        return OutputParseResult(
            ok=True, parsed=value, normalization_reason=normalization_reason
        )


# ---------------------------------------------------------------------------
# OutputContract
# ---------------------------------------------------------------------------

_SIMPLE_TYPES: dict[type, str] = {
    str: "str",
    int: "int",
    float: "float",
    bool: "bool",
    list: "list",
    dict: "dict",
}


class OutputContract:
    """Schema definition for validating model output payloads.

    Contracts check:

    * Required fields are present
    * Field types match (with ``int``-to-``float`` tolerance for numeric fields)
    * Enum values are within the allowed set (case-normalized)
    * Custom validators return errors for context-dependent rules

    Usage::

        contract = OutputContract(
            fields=[
                FieldSchema("root_cause", type_=str, required=True),
                FieldSchema("confidence", type_=(int, float), required=True),
                FieldSchema(
                    "failure_category",
                    type_=str,
                    required=True,
                    enum_values=["tool_error", "verification_failed", ...],
                ),
            ],
            custom_validators=[_my_context_check],
        )
        errors = contract.validate(payload, context={...})
    """

    def __init__(
        self,
        *,
        fields: list[FieldSchema],
        custom_validators: list[Callable[..., list[OutputParseError]]] | None = None,
    ) -> None:
        self._fields: dict[str, FieldSchema] = {f.name: f for f in fields}
        self._custom_validators: list[Callable[..., list[OutputParseError]]] = (
            custom_validators or []
        )

    # ------------------------------------------------------------------
    # public API
    # ------------------------------------------------------------------

    def validate(
        self, payload: dict[str, Any], *, context: dict[str, Any] | None = None
    ) -> list[OutputParseError]:
        """Validate a payload against this contract.

        Args:
            payload: The parsed JSON dict.
            context: Optional context dict passed to custom validators.

        Returns:
            List of errors; empty list means validation passed.
        """
        errors: list[OutputParseError] = []

        # 1. Check required fields present
        for schema_field in self._fields.values():
            if schema_field.required and schema_field.name not in payload:
                errors.append(
                    OutputParseError(
                        code=ERROR_MISSING_REQUIRED_FIELD,
                        message=f"required field '{schema_field.name}' is missing",
                        field=schema_field.name,
                    )
                )

        # 2. Check types for present fields
        for key, value in payload.items():
            schema_field = self._fields.get(key)
            if schema_field is None:
                continue  # unknown fields are allowed (pass through)
            if not self._type_matches(value, schema_field.type_):
                errors.append(
                    OutputParseError(
                        code=ERROR_WRONG_TYPE,
                        message=(
                            f"field '{key}' has wrong type: "
                            f"expected {self._type_label(schema_field.type_)}, "
                            f"got {type(value).__name__}"
                        ),
                        field=key,
                        raw_value_repr=repr(value),
                    )
                )

        # 3. Check enum values
        for schema_field in self._fields.values():
            if schema_field.enum_values is None:
                continue
            raw = payload.get(schema_field.name)
            if raw is None:
                continue
            if isinstance(raw, str):
                normalized = raw.strip().lower().replace("/", "_").replace("-", "_")
                if normalized not in schema_field.enum_values:
                    errors.append(
                        OutputParseError(
                            code=ERROR_ENUM_VIOLATION,
                            message=(
                                f"field '{schema_field.name}' has invalid enum value "
                                f"'{raw}'; allowed: {schema_field.enum_values}"
                            ),
                            field=schema_field.name,
                            raw_value_repr=repr(raw),
                        )
                    )

        # 4. Custom validators
        ctx = context or {}
        for validator in self._custom_validators:
            try:
                custom_errors = validator(payload, ctx)
                errors.extend(custom_errors)
            except Exception:
                # Custom validator failure should not crash the pipeline
                pass

        return errors

    # ------------------------------------------------------------------
    # internal helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _type_label(t: type | tuple[type, ...]) -> str:
        if isinstance(t, tuple):
            return " | ".join(_SIMPLE_TYPES.get(ti, ti.__name__) for ti in t)
        return _SIMPLE_TYPES.get(t, t.__name__)

    @staticmethod
    def _type_matches(value: Any, expected: type | tuple[type, ...]) -> bool:
        # Numeric tolerance: int is acceptable where float is expected
        if isinstance(expected, tuple):
            return any(
                OutputContract._single_type_matches(value, et) for et in expected
            )
        return OutputContract._single_type_matches(value, expected)

    @staticmethod
    def _single_type_matches(value: Any, expected: type) -> bool:
        if expected is float and isinstance(value, int):
            return True  # int → float tolerance
        if expected is int and isinstance(value, bool):
            return False  # bool is not int
        if expected is bool:
            return isinstance(value, bool)  # strict bool
        return isinstance(value, expected)


# ---------------------------------------------------------------------------
# OutputRepairer
# ---------------------------------------------------------------------------


class OutputRepairer:
    """Safe repair of ``OutputContract`` validation errors.

    Rules:

    * Only unambiguous fixes are applied:
      - Strip whitespace from strings
      - Normalize enum case (lowercase + ``/`` → ``_``)
      - Coerce ``int`` → ``float`` for numeric fields
      - Fill ``None`` defaults for missing non-required fields
    * Fields marked ``dangerous=True`` are **never** auto-repaired (fail-closed)
    * Fields with ``allow_repair=False`` are **never** auto-repaired
    * Fields involving commands, paths, permissions, file names, tool names,
      or workspace mutations are treated as dangerous by convention
    """

    def repair(
        self,
        payload: dict[str, Any],
        errors: list[OutputParseError],
        *,
        contract: OutputContract,
    ) -> OutputParseResult:
        """Attempt to repair a payload with validation errors.

        Args:
            payload: The parsed (possibly broken) JSON dict.
            errors: Validation errors from ``OutputContract.validate()``.
            contract: The schema contract used for validation.

        Returns:
            ``OutputParseResult`` — ``ok=True`` with repaired payload if all
            errors were safe to fix; ``ok=False`` if any error was unrepairable.
        """
        repaired = dict(payload)
        repair_errors: list[OutputParseError] = []

        for error in errors:
            field_name = error.field
            if field_name is None:
                # Structural errors (invalid_json, not_object) are not repairable
                repair_errors.append(
                    OutputParseError(
                        code=ERROR_UNSAFE_AUTO_REPAIR,
                        message=f"cannot repair structural error: {error.message}",
                        field=field_name,
                    )
                )
                continue

            field_schema = contract._fields.get(field_name)
            if field_schema is None:
                # Unknown field — skip
                continue

            if field_schema.dangerous or not field_schema.allow_repair:
                repair_errors.append(
                    OutputParseError(
                        code=ERROR_UNSAFE_AUTO_REPAIR,
                        message=(
                            f"field '{field_name}' is marked dangerous or "
                            f"repair-disabled; cannot auto-repair: {error.message}"
                        ),
                        field=field_name,
                    )
                )
                continue

            # --- actually attempt repair ---
            if error.code == ERROR_MISSING_REQUIRED_FIELD:
                # Cannot invent required values
                repair_errors.append(
                    OutputParseError(
                        code=ERROR_UNSAFE_AUTO_REPAIR,
                        message=f"cannot invent value for required field '{field_name}'",
                        field=field_name,
                    )
                )
                continue

            elif error.code == ERROR_WRONG_TYPE:
                fixed = self._repair_type(
                    repaired.get(field_name), field_schema.type_
                )
                if fixed is not None:
                    repaired[field_name] = fixed
                else:
                    repair_errors.append(
                        OutputParseError(
                            code=ERROR_UNSAFE_AUTO_REPAIR,
                            message=f"cannot coerce type for field '{field_name}': {error.message}",
                            field=field_name,
                        )
                    )

            elif error.code == ERROR_ENUM_VIOLATION:
                raw = repaired.get(field_name)
                if isinstance(raw, str) and field_schema.enum_values:
                    normalized = raw.strip().lower().replace("/", "_").replace("-", "_")
                    if normalized in field_schema.enum_values:
                        repaired[field_name] = normalized
                    else:
                        repair_errors.append(
                            OutputParseError(
                                code=ERROR_UNSAFE_AUTO_REPAIR,
                                message=(
                                    f"cannot map enum value '{raw}' to "
                                    f"allowed set for field '{field_name}'"
                                ),
                                field=field_name,
                            )
                        )
                else:
                    repair_errors.append(error)

            else:
                # Other error codes are not repairable by this repairer
                repair_errors.append(error)

        if repair_errors:
            return OutputParseResult(ok=False, parsed=None, errors=repair_errors)

        # Apply trimmable defaults: strip string values
        for key, value in repaired.items():
            if isinstance(value, str):
                repaired[key] = value.strip()

        return OutputParseResult(ok=True, parsed=repaired)

    # ------------------------------------------------------------------
    # internal helpers
    # ------------------------------------------------------------------

    @staticmethod
    def _repair_type(value: Any, expected: type | tuple[type, ...]) -> Any:
        """Attempt to coerce *value* to *expected* type. Returns None if impossible."""
        if value is None:
            return None

        def matches_single(v: Any, t: type) -> bool:
            if t is float and isinstance(v, (int, float)):
                return True
            if t is int and isinstance(v, int) and not isinstance(v, bool):
                return True
            if t is bool and isinstance(v, bool):
                return True
            return isinstance(v, t)

        targets = expected if isinstance(expected, tuple) else (expected,)

        # Already matches one of the targets
        for t in targets:
            if matches_single(value, t):
                return value

        # int → float coercion
        if float in targets and isinstance(value, int) and not isinstance(value, bool):
            return float(value)

        # float → int coercion (truncation)
        if int in targets and isinstance(value, float):
            return int(value)

        # str → number coercion (safe for numeric-looking strings)
        if isinstance(value, str):
            stripped = value.strip()
            if int in targets:
                try:
                    return int(stripped)
                except ValueError:
                    pass
            if float in targets:
                try:
                    return float(stripped)
                except ValueError:
                    pass

        # bool coercion (only from actual bools)
        if bool in targets and isinstance(value, bool):
            return value

        # list coercion from tuple
        if list in targets and isinstance(value, (tuple, list)):
            return list(value)

        return None


# ---------------------------------------------------------------------------
# OutputGuardrail
# ---------------------------------------------------------------------------


class OutputGuardrail:
    """Safety boundary checks that run after parsing and repair.

    These checks enforce fail-closed rules for dangerous fields:

    * ``affected_files`` must not escape the workspace root
    * ``verification_plan`` / ``verification_requirements`` must contain
      executable commands — never guessed
    * Tool / capability references must be in allowed sets (when provided)

    Guardrail failures produce ``unauthorized_reference`` or
    ``unsafe_auto_repair`` errors and should trigger fallback.
    """

    def check(
        self,
        payload: dict[str, Any],
        *,
        contract: OutputContract,
        context: dict[str, Any] | None = None,
    ) -> list[OutputParseError]:
        """Run guardrail checks on a payload.

        Args:
            payload: The parsed/repaired JSON dict.
            contract: The schema contract (identifies dangerous fields).
            context: Optional context with ``workspace_root``,
                ``allowed_target_files``, ``allowed_tool_names``, etc.

        Returns:
            List of guardrail errors; empty means all checks passed.
        """
        errors: list[OutputParseError] = []
        ctx = context or {}

        for schema_field in contract._fields.values():
            if not schema_field.dangerous:
                continue
            value = payload.get(schema_field.name)
            if value is None:
                continue

            # --- affected_files guardrail ---
            if schema_field.name == "affected_files" and isinstance(value, list):
                workspace_root = ctx.get("workspace_root", "")
                allowed = set(ctx.get("allowed_target_files") or [])
                for raw_path in value:
                    if not isinstance(raw_path, str):
                        errors.append(
                            OutputParseError(
                                code=ERROR_UNAUTHORIZED_REFERENCE,
                                message=(
                                    f"affected_files contains non-string entry: "
                                    f"{raw_path!r}"
                                ),
                                field=schema_field.name,
                            )
                        )
                        continue
                    normalized = self._normalize_path(raw_path, workspace_root)
                    if normalized is None:
                        errors.append(
                            OutputParseError(
                                code=ERROR_UNAUTHORIZED_REFERENCE,
                                message=(
                                    f"affected_files path escapes workspace: "
                                    f"'{raw_path}'"
                                ),
                                field=schema_field.name,
                            )
                        )
                    elif allowed and normalized not in allowed:
                        errors.append(
                            OutputParseError(
                                code=ERROR_UNAUTHORIZED_REFERENCE,
                                message=(
                                    f"affected_files path not in allowed targets: "
                                    f"'{raw_path}'"
                                ),
                                field=schema_field.name,
                            )
                        )

            # --- verification_plan / command guardrail ---
            elif schema_field.name in ("verification_plan", "verification_requirements"):
                if isinstance(value, list) and len(value) == 0:
                    if (
                        schema_field.name == "verification_plan"
                        and payload.get("needs_user_input") is True
                        and isinstance(payload.get("blocked_reason"), str)
                        and payload["blocked_reason"].strip()
                    ):
                        continue
                    errors.append(
                        OutputParseError(
                            code=ERROR_UNSAFE_AUTO_REPAIR,
                            message=(
                                f"'{schema_field.name}' is empty; verification commands "
                                f"must not be guessed"
                            ),
                            field=schema_field.name,
                        )
                    )

        return errors

    @staticmethod
    def _normalize_path(raw_path: str, workspace_root: str) -> str | None:
        """Normalize a path and check it stays within workspace_root.

        Returns the normalized relative path, or None if it escapes.
        """
        import os

        raw = raw_path.strip().replace("\\", "/")
        if not raw:
            return None
        # Resolve relative to workspace
        if os.path.isabs(raw):
            joined = os.path.normpath(raw)
        else:
            joined = os.path.normpath(
                os.path.join(workspace_root.replace("\\", "/"), raw)
            )
        norm_root = os.path.normpath(workspace_root.replace("\\", "/"))
        # Check containment
        if not joined.startswith(norm_root + os.sep) and joined != norm_root:
            return None
        # Return relative path
        rel = os.path.relpath(joined, norm_root)
        return rel.replace("\\", "/")


# ---------------------------------------------------------------------------
# Predefined contracts
# ---------------------------------------------------------------------------


FAILURE_ANALYSIS_OUTPUT_CONTRACT = OutputContract(
    fields=[
        FieldSchema("analysis_id", type_=str, required=False),
        FieldSchema("root_cause", type_=str, required=True),
        FieldSchema("failure_category", type_=str, required=True),
        FieldSchema(
            "affected_files",
            type_=list,
            required=False,
            dangerous=True,
            allow_repair=False,
        ),
        FieldSchema("evidence_refs", type_=list, required=True),
        FieldSchema("repair_strategy", type_=str, required=True),
        FieldSchema("next_actions", type_=list, required=True),
        FieldSchema(
            "verification_plan",
            type_=list,
            required=False,
            dangerous=True,
            allow_repair=False,
        ),
        FieldSchema("confidence", type_=(int, float), required=True),
        FieldSchema("needs_user_input", type_=bool, required=True),
        FieldSchema("blocked_reason", type_=str, required=False),
    ],
)

TASK_CONTRACT_OUTPUT_CONTRACT = OutputContract(
    fields=[
        FieldSchema("user_goal", type_=str, required=True),
        FieldSchema("acceptance_criteria", type_=list, required=True),
        FieldSchema("deliverables", type_=list, required=False),
        FieldSchema(
            "verification_requirements",
            type_=list,
            required=False,
            dangerous=True,
            allow_repair=False,
        ),
        FieldSchema("constraints", type_=list, required=False),
        FieldSchema("report_requirements", type_=list, required=False),
        FieldSchema("evidence_requirements", type_=list, required=False),
    ],
)

SEMANTIC_PLAN_OUTPUT_CONTRACT = OutputContract(
    fields=[
        FieldSchema("rolling_plan", type_=dict, required=True),
        FieldSchema("risk_points", type_=list, required=False),
        FieldSchema(
            "verification_strategies",
            type_=list,
            required=False,
            dangerous=True,
            allow_repair=False,
        ),
        FieldSchema("repair_policy", type_=dict, required=False),
    ],
)

PLANNER_DECISION_OUTPUT_CONTRACT = OutputContract(
    fields=[
        FieldSchema("decision", type_=str, required=True),
        FieldSchema("reason", type_=str, required=True),
        FieldSchema("next_action", type_=str, required=False),
        FieldSchema("risk_points_triggered", type_=list, required=False),
        FieldSchema("verification_strategy_selected", type_=str, required=False),
    ],
)
