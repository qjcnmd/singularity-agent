from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

from singularity.evaluation.models import (
    EvaluationProfile,
    TraceReplayResult,
    canonical_json,
)
from singularity.observability.models import TraceEvent, TraceEventType


SIDE_EFFECT_EVENT_TYPES = {
    TraceEventType.COMMAND_STARTED.value,
    TraceEventType.COMMAND_COMPLETED.value,
    TraceEventType.MUTATION_APPLIED.value,
    TraceEventType.EDIT_APPLIED.value,
    TraceEventType.PATCH_PROPOSED.value,
}


class TraceReplayHarness:
    def __init__(self, *, project_root: Path | str) -> None:
        self.project_root = Path(project_root)

    def replay(
        self,
        trace_run_dir: Path | str,
        *,
        profile: EvaluationProfile,
        replay_policy: dict[str, Any] | None = None,
    ) -> TraceReplayResult:
        trace_run_dir = Path(trace_run_dir)
        replay_policy = replay_policy or {"side_effects": "simulate"}
        raw_events = _read_jsonl(trace_run_dir / "events.jsonl")
        raw_spans = _read_jsonl(trace_run_dir / "spans.jsonl")
        raw_artifacts = _read_jsonl(trace_run_dir / "artifacts.jsonl")
        trace_input_digest = _trace_input_digest(trace_run_dir)
        events = [TraceEvent.from_dict(item) for item in raw_events]
        side_effects = [
            event for event in events if event.event_type.value in SIDE_EFFECT_EVENT_TYPES
        ]
        tool_events = [
            event
            for event in events
            if event.event_type
            in {
                TraceEventType.TOOL_DISPATCH_COMPLETED,
                TraceEventType.TOOL_PROTOCOL_CALL_COMPLETED,
            }
        ]
        allowed_tools = set(profile.allowed_tools)
        profile_policy_violations = len(
            [
                event
                for event in tool_events
                if allowed_tools
                and _tool_name(event)
                and _tool_name(event) not in allowed_tools
            ]
        )
        policy_denials = len(
            [
                event
                for event in events
                if event.event_type
                in {
                    TraceEventType.POLICY_BLOCKED,
                    TraceEventType.APPROVAL_DENIED,
                    TraceEventType.TOOL_PROTOCOL_CALL_REJECTED,
                }
            ]
        )
        verification = _verification_from_events(events)
        usage = _usage_from_events(events)
        metrics = {
            "events": len(raw_events),
            "spans": len(raw_spans),
            "artifacts": len(raw_artifacts),
            "tool_calls": len(tool_events),
            "policy_denials": policy_denials + profile_policy_violations,
            "trace_policy_denials": policy_denials,
            "profile_policy_violations": profile_policy_violations,
            "interventions": len(
                [
                    event
                    for event in events
                    if event.event_type
                    in {
                        TraceEventType.APPROVAL_REQUESTED,
                        TraceEventType.CLARIFICATION_REQUESTED,
                        TraceEventType.USER_DECISION_RECORDED,
                    }
                ]
            ),
            "tool_failures": len(
                [event for event in events if event.event_type == TraceEventType.TOOL_DISPATCH_FAILED]
            ),
            "latency_ms": usage.get("latency_ms", 0),
            "cost": usage.get("cost", 0.0),
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "side_effect_events": len(side_effects),
            "trace_input_digest": trace_input_digest,
        }
        classification = "simulated_side_effects" if side_effects else "read_only_replay"
        fingerprint_payload = {
            "profile": profile.config_fingerprint_payload(),
            "replay_policy": replay_policy,
            "trace_input_digest": trace_input_digest,
        }
        config_fingerprint = _sha256(canonical_json(fingerprint_payload))
        stable_payload = {
            "classification": classification,
            "config_fingerprint": config_fingerprint,
            "metrics": metrics,
            "trace_input_digest": trace_input_digest,
            "verification": verification,
        }
        return TraceReplayResult(
            trace_run_dir=trace_run_dir,
            profile=profile,
            deterministic=True,
            replay_classification=classification,
            metrics=metrics,
            verification=verification,
            events_replayed=len(events),
            side_effects_simulated=len(side_effects),
            config_fingerprint=config_fingerprint,
            trace_input_digest=trace_input_digest,
            result_hash=_sha256(canonical_json(stable_payload)),
        )


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    rows: list[dict[str, Any]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if line.strip():
            rows.append(json.loads(line))
    return rows


def _trace_input_digest(trace_run_dir: Path) -> str:
    hasher = hashlib.sha256()
    for name in ("events.jsonl", "spans.jsonl", "artifacts.jsonl"):
        path = trace_run_dir / name
        hasher.update(name.encode("utf-8"))
        hasher.update(b"\0")
        if path.exists():
            hasher.update(path.read_bytes())
        else:
            hasher.update(b"<missing>")
        hasher.update(b"\0")
    return hasher.hexdigest()


def _verification_from_events(events: list[TraceEvent]) -> dict[str, Any]:
    verification_events = [
        event
        for event in events
        if event.event_type
        in {
            TraceEventType.VERIFICATION_CHECK_COMPLETED,
            TraceEventType.VERIFICATION_FAILED,
        }
    ]
    if not verification_events:
        return {"status": "unknown", "passed": 0, "failed": 0}
    failed = 0
    passed = 0
    for event in verification_events:
        status = str(event.payload.get("status", "")).lower()
        if event.event_type == TraceEventType.VERIFICATION_FAILED or status in {"failed", "failure"}:
            failed += 1
        elif status in {"passed", "ready", "success"}:
            passed += 1
    return {
        "status": "failed" if failed else "ready",
        "passed": passed,
        "failed": failed,
    }


def _usage_from_events(events: list[TraceEvent]) -> dict[str, Any]:
    input_tokens = 0
    output_tokens = 0
    cost = 0.0
    latency_ms = 0
    for event in events:
        if event.event_type != TraceEventType.MODEL_RESPONSE_RECEIVED:
            continue
        usage = event.payload.get("usage") or {}
        input_tokens += _safe_int(usage.get("input_tokens"))
        output_tokens += _safe_int(usage.get("output_tokens"))
        cost += _safe_float(usage.get("cost_estimate", usage.get("cost")))
        latency_ms += _safe_int(event.payload.get("latency_ms"))
    return {
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
        "cost": round(cost, 6),
        "latency_ms": latency_ms,
    }


def _tool_name(event: TraceEvent) -> str | None:
    name = event.payload.get("tool_name")
    if name:
        return str(name)
    name = event.payload.get("name")
    if name:
        return str(name)
    return None


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def _safe_int(value: Any) -> int:
    try:
        return int(value or 0)
    except (TypeError, ValueError):
        return 0


def _safe_float(value: Any) -> float:
    try:
        return float(value or 0.0)
    except (TypeError, ValueError):
        return 0.0
