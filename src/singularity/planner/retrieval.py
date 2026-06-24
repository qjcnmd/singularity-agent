from __future__ import annotations

from typing import Any

from singularity.planner.models import TaskStatus


class RetrievalOrchestrator:
    def retrieve(
        self,
        *,
        current_step: Any | None = None,
        failure_analysis: Any | None = None,
        changed_files: list[str] | None = None,
        task_contract: dict[str, Any] | None = None,
        project_index: Any | None = None,
        trigger: str = "manual",
    ) -> dict[str, Any]:
        analysis = _plain(failure_analysis)
        step = _plain(current_step)
        contract = task_contract or {}
        changed = _strings(changed_files or [])
        files_to_read: list[str] = []
        index_queries: list[str] = []
        memory_queries: list[str] = []
        evidence_sources: list[str] = []

        for path in changed:
            _append_unique(files_to_read, path)
        for path in _strings(analysis.get("suspect_files") or []):
            _append_unique(files_to_read, path)
        for query in _strings(analysis.get("retrieval_queries") or []):
            _append_unique(index_queries, query)

        goal = str(contract.get("user_goal") or "")
        if not index_queries and step.get("title"):
            _append_unique(index_queries, step["title"])
        if goal:
            _append_unique(memory_queries, goal)
            if not index_queries:
                _append_unique(index_queries, goal)
        for query in index_queries:
            _append_unique(memory_queries, query)

        if analysis.get("analysis_id"):
            evidence_sources.append(f"failure_analysis:{analysis['analysis_id']}")
        impact = _project_index_impact(project_index, changed)
        if impact:
            evidence_sources.append("project_index:impact")
            for path in _strings(impact.get("reverse_dependencies") or []):
                _append_unique(files_to_read, path)
            for path in _strings(impact.get("affected_tests") or []):
                _append_unique(files_to_read, path)
        test_impact = _project_index_tests(project_index, changed)
        if test_impact:
            evidence_sources.append("project_index:test_impact")
            for path in _strings(test_impact.get("likely_tests") or []):
                _append_unique(files_to_read, path)

        return {
            "trigger": trigger,
            "current_step_id": step.get("step_id"),
            "files_to_read": files_to_read[:20],
            "index_queries": index_queries[:10],
            "memory_queries": memory_queries[:10],
            "changed_files": changed,
            "project_index": {
                "impact": impact,
                "test_impact": test_impact,
            },
            "evidence_sources": evidence_sources,
            "trust_level": "component_generated",
        }


class LessonExtractor:
    def extract(
        self,
        final_report: Any,
        *,
        memory_pipeline: Any | None,
        accept: bool = False,
    ) -> list[Any]:
        if memory_pipeline is None or not hasattr(memory_pipeline, "ingest_final_report"):
            return []
        payload = _plain(final_report)
        if not _verified_completed(payload):
            return []
        return list(memory_pipeline.ingest_final_report(final_report, accept=accept))


def _verified_completed(report: dict[str, Any]) -> bool:
    status = str(report.get("status") or "").lower()
    verification = report.get("verification_summary") or {}
    verification_status = str(verification.get("status") or "").lower()
    return status == TaskStatus.COMPLETED.value and verification_status in {
        "ready",
        "ready_with_warnings",
        "passed",
        "completed",
    }


def _project_index_impact(project_index: Any | None, changed_files: list[str]) -> dict[str, Any]:
    if project_index is None or not changed_files or not hasattr(project_index, "analyze_impact"):
        return {}
    try:
        return _plain(project_index.analyze_impact(changed_files))
    except Exception:
        return {}


def _project_index_tests(project_index: Any | None, changed_files: list[str]) -> dict[str, Any]:
    if project_index is None or not changed_files or not hasattr(project_index, "get_test_impact"):
        return {}
    try:
        return _plain(project_index.get_test_impact(changed_files))
    except Exception:
        return {}


def _plain(value: Any) -> dict[str, Any]:
    if value is None:
        return {}
    if hasattr(value, "to_dict"):
        return value.to_dict()
    if isinstance(value, dict):
        return dict(value)
    return {}


def _strings(values: list[Any]) -> list[str]:
    return [str(value) for value in values if value]


def _append_unique(values: list[str], value: Any) -> None:
    if value is None:
        return
    text = str(value)
    if text and text not in values:
        values.append(text)
