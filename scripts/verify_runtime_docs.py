from __future__ import annotations

import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
DOCS_DIR = REPO_ROOT / "docs" / "architecture" / "modules"


def _ensure_utf8_stdio() -> None:
    """Keep Chinese verifier output printable on Windows CI consoles."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if callable(reconfigure):
            reconfigure(encoding="utf-8", errors="replace")


CORE_DOC_IDS = {
    "agent-loop",
    "kernel-agent-graph",
    "model-turn-provider-tools",
    "context-assembly-prompt-frame",
    "context-compaction-observation-store",
    "tool-registry-exposure",
    "tool-execution",
    "policy-approval-gates",
    "planner-replanner-failure-recovery",
    "failure-analysis-repair",
    "verification-contract-satisfaction",
    "trace-observation-audit-events",
    "evaluation-benchmark-runner",
    "plugin-tools-registry",
    "command-execution",
    "sandbox-isolation",
    "memory-index-context",
    "artifact-long-result-handling",
    "session-recovery",
}

REQUIRED_HEADINGS = {
    "这一层解决什么问题",
    "当前源码位置",
    "关键类、函数、字段",
    "真实运行时调用链",
    "真实任务中的对象流",
    "真实对象完整结构",
    "谁生成这些对象",
    "谁消费这些对象",
    "是否落盘",
    "是否进入 trace / audit",
    "失败路径",
    "当前结构问题",
    "维护规则",
}

FORBIDDEN_TEMPLATE_PHRASES = {
    "这些对象由上文列出的源码组件在运行链路中生成",
    "生成动作必须来自当前源码路径",
    "消费方是同一调用链后续组件",
    "落盘只通过当前源码中的",
    "进入 trace / audit 的内容以",
    "失败路径由当前源码中的异常",
    "当前结构仍大量使用字典 payload 连接组件",
    "关键符号见本文顶部",
    "真实对象字段见本文顶部",
    "对象流小节会直接引用这些名字",
    "字段完整性由脚本校验",
}

REAL_TASK_FLOW_MARKERS = {
    "trace",
    "sqlite",
    "jsonl",
    "artifact",
    "report",
    "workspace",
    "context",
    "audit",
    "store",
}

COMPLETE_FIELD_CHECKS = {
    "agent-loop": {
        "AgentLoopResult",
    },
    "kernel-agent-graph": {
        "AgentGraph",
        "RunIdentity",
        "AgentRun",
        "AgentSession",
        "KernelContext",
        "LifecycleEvent",
    },
    "model-turn-provider-tools": {
        "ContentBlock",
        "ModelMessage",
        "ModelToolSchema",
        "ToolChoicePolicy",
        "ModelToolCall",
        "ModelCapabilities",
        "ModelPreferences",
        "ModelBudget",
        "ModelUsage",
        "ModelTurnRequest",
        "ModelValidationResult",
        "ModelError",
        "ModelTurnResult",
    },
    "context-assembly-prompt-frame": {
        "ContextReference",
        "ContextItem",
        "ContextBudgetPlan",
        "ContextRenderPolicy",
        "ContextBundle",
        "ContextUsageReport",
        "ContextSnapshot",
        "ToolObservation",
        "PlannerState",
        "PolicyObservation",
        "VerificationEvidence",
        "MutationEvidence",
        "CommandObservation",
        "CacheAttribution",
        "ContextSummaryPayload",
        "ContextSummaryEnvelope",
        "PromptManifest",
        "PromptBundle",
    },
    "context-compaction-observation-store": {
        "ContextSnapshot",
        "ToolObservation",
        "RecoveredContext",
        "PartialCompactionRange",
    },
    "tool-registry-exposure": {
        "ToolSpec",
        "ToolResult",
        "ToolExecutionRequest",
        "RegisteredToolRecord",
        "ToolOrigin",
        "ToolCachePolicy",
        "ToolIdempotencyPolicy",
        "ToolRetryPolicy",
    },
    "tool-execution": {
        "ToolCallEnvelope",
        "ToolCallBatch",
        "ToolExecutionPlan",
        "ToolCallRecord",
        "ToolProtocolResultEnvelope",
        "ToolProtocolTurnResult",
        "ToolProtocolValidationResult",
        "ToolProtocolRecoveryReport",
        "ToolProtocolEvent",
        "ToolProtocolResultBinding",
    },
    "policy-approval-gates": {
        "PolicySubject",
        "ResourceRef",
        "PolicyConstraints",
        "PolicyRequest",
        "ApprovalScope",
        "ApprovalRequirement",
        "ApprovalGrant",
        "PolicyDecision",
        "PolicyAuditEntry",
    },
    "planner-replanner-failure-recovery": {
        "CompletionCriteria",
        "TaskState",
        "TaskPhase",
        "TaskPlan",
        "AgentAction",
        "EvidenceLedger",
        "ExecutionBudget",
        "AuthorizationDecision",
        "ReplanDecision",
        "RiskEscalation",
        "FinalReport",
    },
    "failure-analysis-repair": {
        "FailureAnalysisRequest",
        "FailureAnalysisResult",
        "RepairContract",
        "RepairActionCandidate",
        "RepairPlan",
        "RepairReplanSignal",
    },
    "verification-contract-satisfaction": {
        "VerificationCheck",
        "VerificationResult",
        "CompletionAssessment",
        "VerificationStep",
        "VerificationContract",
        "StepEvidence",
        "ContractSatisfaction",
    },
    "trace-observation-audit-events": {
        "TraceEvent",
        "TraceSpan",
        "TraceArtifact",
        "TraceTimelineItem",
        "TraceSummary",
    },
    "evaluation-benchmark-runner": {
        "EvaluationWorkspace",
        "EvaluationTask",
        "EvaluationTaskSet",
        "CommandEvalResult",
        "EvaluationTaskResult",
        "TargetedFailureReplayResult",
    },
    "plugin-tools-registry": {
        "CompatibilitySpec",
        "PluginManifest",
        "DiscoveredPlugin",
        "PluginDiagnostic",
        "PluginStatus",
        "PluginLockEntry",
        "PluginToolContribution",
        "PluginContributionSet",
        "PluginLoadResult",
    },
    "command-execution": {
        "ResourceLimits",
        "CommandRequest",
        "CommandPolicyResult",
        "CommandPlan",
        "CommandResult",
        "ProcessSession",
        "ProcessOutput",
        "ProcessStopResult",
    },
    "sandbox-isolation": {
        "SandboxCapabilities",
        "SandboxResourceLimits",
        "SandboxEnvPolicy",
        "SandboxFilesystemPolicy",
        "SandboxNetworkPolicy",
        "SandboxProfile",
        "SandboxRequest",
        "PreparedSandbox",
        "SandboxArtifact",
        "SandboxChangeSummary",
        "SandboxViolation",
        "SandboxResult",
    },
    "memory-index-context": {
        "MemoryEvidenceRef",
        "Provenance",
        "TTL",
        "MemoryEntry",
        "MemoryCandidate",
        "MemoryQuery",
        "MemorySearchResult",
        "MemoryContextBlock",
    },
    "artifact-long-result-handling": {
        "TraceArtifact",
    },
    "session-recovery": {
        "RecoveryGateDecision",
        "SessionCheckpoint",
        "SessionDetail",
        "SessionLaunch",
        "SessionResumeContext",
        "SessionRun",
        "SessionSummary",
        "SessionTimelineEvent",
    },
}

SCAN_SUFFIXES = {".md", ".py", ".json", ".toml", ".yaml", ".yml"}
SKIP_DIRS = {
    ".git",
    ".singularity",
    ".venv",
    ".pytest_cache",
    ".ruff_cache",
    ".mypy_cache",
    "outputs",
    "work",
    "__pycache__",
}
RETIRED_DOC_PATHS = {
    "docs/" + "adr",
    "docs/evaluation" + "-harness.md",
    "docs/INSTALLATION" + "_LAYOUT.md",
    "docs/UPGRADE" + "_AND_MIGRATION.md",
    "docs/architecture/agent" + "-host-transition.md",
    "docs/architecture/agent" + "-kernel.md",
    "docs/architecture/boundary" + "-contracts.md",
    "docs/architecture/code" + "-index.md",
    "docs/architecture/command" + "-execution.md",
    "docs/architecture/desktop" + "-architecture-strategy.md",
    "docs/architecture/edit" + "-execution.md",
    "docs/architecture/event" + "-model.md",
    "docs/architecture/execution" + "-map.md",
    "docs/architecture/migration" + "-to-desktop.md",
    "docs/architecture/model" + "-runner.md",
    "docs/architecture/naming" + "-and-concept-map.md",
    "docs/architecture/naming.md",
    "docs/architecture/observability" + "-tracing.md",
    "docs/architecture/planning" + "-and-run-control.md",
    "docs/architecture/plugin" + "-management.md",
    "docs/architecture/policy" + "-approval-engine.md",
    "docs/architecture/policy" + "-approval.md",
    "docs/architecture/prompt" + "-assembly.md",
    "docs/architecture/sandbox" + "-isolation.md",
    "docs/architecture/state" + "-model.md",
    "docs/architecture/tool" + "-execution.md",
    "docs/architecture/tool" + "-protocol.md",
    "docs/architecture/trace" + "-audit.md",
    "docs/architecture/verification" + "-runner.md",
    "docs/architecture/workspace" + "-mutation.md",
    "docs/architecture/workspace" + "-state-checkpointing.md",
}


@dataclass(frozen=True)
class ModuleDataFlowDoc:
    path: Path
    text: str
    doc_id: str
    source_paths: list[str]
    symbols: list[str]
    field_checks: dict[str, list[str]]
    headings: set[str]


def main() -> int:
    errors: list[str] = []
    docs = _load_docs(errors)
    seen_ids = {doc.doc_id for doc in docs if doc.doc_id}

    missing = sorted(CORE_DOC_IDS - seen_ids)
    for doc_id in missing:
        errors.append(f"missing core module data-flow doc id: {doc_id}")

    duplicate_ids = sorted(
        doc_id for doc_id in seen_ids if sum(1 for doc in docs if doc.doc_id == doc_id) > 1
    )
    for doc_id in duplicate_ids:
        errors.append(f"duplicate module data-flow doc id: {doc_id}")

    for doc in docs:
        _verify_doc(doc, errors)

    _verify_doc_tree(errors)
    _verify_forbidden_terms(errors)

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(f"模块数据流文档校验通过：{len(docs)} 个文档，{len(CORE_DOC_IDS)} 个核心模块已覆盖。")
    return 0


def _load_docs(errors: list[str]) -> list[ModuleDataFlowDoc]:
    if not DOCS_DIR.is_dir():
        errors.append(f"module data-flow docs directory missing: {_rel(DOCS_DIR)}")
        return []

    docs: list[ModuleDataFlowDoc] = []
    for path in sorted(DOCS_DIR.glob("*.md")):
        text = path.read_text(encoding="utf-8")
        doc_id = _extract_doc_id(text)
        docs.append(
            ModuleDataFlowDoc(
                path=path,
                text=text,
                doc_id=doc_id,
                source_paths=_extract_list(text, "源码证据路径:"),
                symbols=_extract_list(text, "关键符号:"),
                field_checks=_extract_field_checks(text),
                headings=_extract_headings(text),
            )
        )
    return docs


def _verify_doc(doc: ModuleDataFlowDoc, errors: list[str]) -> None:
    label = _rel(doc.path)
    if "runtime" in doc.path.stem.lower():
        errors.append(f"{label}: filename must not use runtime as module naming")
    if not doc.doc_id:
        errors.append(f"{label}: missing '模块数据流文档 ID:'")

    missing_headings = sorted(REQUIRED_HEADINGS - doc.headings)
    for heading in missing_headings:
        errors.append(f"{label}: missing heading '## {heading}'")

    if not doc.source_paths:
        errors.append(f"{label}: no 源码证据路径 entries")
    if not doc.symbols:
        errors.append(f"{label}: no 关键符号 entries")

    for phrase in sorted(FORBIDDEN_TEMPLATE_PHRASES):
        if phrase in doc.text:
            errors.append(f"{label}: forbidden template phrase remains: {phrase}")

    flow_section = _extract_section(doc.text, "## 真实任务中的对象流")
    if not flow_section:
        errors.append(f"{label}: missing or empty '## 真实任务中的对象流' section")
    else:
        _verify_flow_section(label, flow_section, errors)

    existing_sources: list[Path] = []
    for source in doc.source_paths:
        source_path = (REPO_ROOT / source).resolve(strict=False)
        if not source_path.exists():
            errors.append(f"{label}: source path does not exist: {source}")
            continue
        existing_sources.append(source_path)

    available = _symbols_in_sources(existing_sources)
    for symbol in doc.symbols:
        if symbol not in available:
            errors.append(f"{label}: symbol not found in listed source paths: {symbol}")

    class_fields = _class_fields_in_sources(existing_sources)
    for class_name, documented in doc.field_checks.items():
        if class_name not in class_fields:
            errors.append(f"{label}: field check class not found in listed source paths: {class_name}")
            continue
        actual = class_fields[class_name]
        documented_set = set(documented)
        missing = sorted(actual - documented_set)
        extra = sorted(documented_set - actual)
        if missing:
            errors.append(
                f"{label}: 字段清单 for {class_name} missing current fields: "
                + ", ".join(missing)
            )
        if extra:
            errors.append(
                f"{label}: 字段清单 for {class_name} lists non-source fields: "
                + ", ".join(extra)
            )

    complete_classes = COMPLETE_FIELD_CHECKS.get(doc.doc_id, set())
    for class_name in sorted(complete_classes):
        if class_name not in class_fields:
            errors.append(f"{label}: complete field check class not found in listed source paths: {class_name}")
            continue
        if class_name not in doc.field_checks:
            errors.append(f"{label}: complete field check missing 字段清单 entry: {class_name}")

    _verify_deep_dive_section(doc, errors)
    _verify_doc_specific_contracts(doc, errors)


def _verify_doc_specific_contracts(doc: ModuleDataFlowDoc, errors: list[str]) -> None:
    label = _rel(doc.path)
    removed_prompt_entrypoint = "PromptAssemblyPipeline." + "build()"
    if doc.doc_id == "context-assembly-prompt-frame" and removed_prompt_entrypoint in doc.text:
        errors.append(
            f"{label}: PromptAssemblyPipeline has no build() entrypoint; "
            "document build_for_model_turn()/build_prompt_bundle() instead"
        )


def _verify_deep_dive_section(doc: ModuleDataFlowDoc, errors: list[str]) -> None:
    """Verify that the doc contains deep-dive inline object definitions, not just field-list pointers."""
    label = _rel(doc.path)
    structure_section = _extract_section(doc.text, "## 真实对象完整结构")
    if not structure_section:
        errors.append(f"{label}: missing or empty '## 真实完整结构' section")
        return

    # Must contain at least one code block with type-annotated fields
    code_blocks = re.findall(r"```[\s\S]*?```", structure_section)
    has_typed_field = False
    for block in code_blocks:
        if re.search(r"\w+\s*:\s*[A-Z]\w+", block):
            has_typed_field = True
            break
    if not has_typed_field:
        errors.append(
            f"{label}: '真实对象完整结构' must contain at least one code block "
            "with type-annotated field definitions (e.g. `field: Type`)"
        )

    # Must mention at least 2 class names from COMPLETE_FIELD_CHECKS for this doc
    # (or all available if fewer than 2)
    complete_classes = COMPLETE_FIELD_CHECKS.get(doc.doc_id, set())
    mentioned = sum(1 for cls in complete_classes if cls in structure_section)
    min_required = min(2, len(complete_classes))
    if mentioned < min_required:
        errors.append(
            f"{label}: '真实对象完整结构' must mention at least 2 key class names, "
            f"found {mentioned}"
        )

    # Must show at least one enum value domain (either in code block or prose)
    has_enum_values = bool(re.search(r'[A-Z_]+\s*=\s*"[a-z_]+"', structure_section))
    has_enum_prose = bool(re.search(r"枚举值[包括为]", structure_section))
    if not has_enum_values and not has_enum_prose:
        errors.append(
            f"{label}: '真实对象完整结构' must show at least one enum value domain "
            '(e.g. `VALUE = "value"` or prose listing enum values)'
        )

    # No omission markers allowed in code blocks
    _OMISSION_PATTERNS = [
        r"\.\.\.\s*\d*\s*more\s+(fields|members|values)",
        r"\.\.\.\s*\d*\s*more\b",
        r"\betc\.\s*\)",
        r"#\s*\.\.\.",
    ]
    for block in code_blocks:
        for pattern in _OMISSION_PATTERNS:
            if re.search(pattern, block, re.IGNORECASE):
                errors.append(
                    f"{label}: code block in '真实对象完整结构' contains omission marker "
                    f"('{pattern}'); every field and enum member must be listed in full"
                )
                break

    # Verify producer/consumer/persistence/trace specificity
    _verify_section_specificity(label, "谁生成这些对象", doc.text, errors,
                                r"\w+\.\w+\(|def \w+", "at least one concrete method name")
    _verify_section_specificity(label, "谁消费这些对象", doc.text, errors,
                                r"\w+\.\w+\(|def \w+", "at least one concrete method name")
    _verify_section_specificity(label, "是否落盘", doc.text, errors,
                                r"(sqlite|jsonl|\.json|\.md|\.txt|artifact|store|落盘)",
                                "at least one concrete storage path or store name")
    _verify_section_specificity(label, "是否进入 trace / audit", doc.text, errors,
                                r"(event|trace|audit|jsonl|\.sqlite|span|artifact)",
                                "at least one concrete trace/audit reference")


def _verify_section_specificity(
    label: str,
    heading: str,
    text: str,
    errors: list[str],
    pattern: str,
    description: str,
) -> None:
    section = _extract_section(text, f"## {heading}")
    if not section:
        return
    if not re.search(pattern, section):
        errors.append(f"{label}: '## {heading}' section must contain {description}")


def _verify_doc_tree(errors: list[str]) -> None:
    docs_root = REPO_ROOT / "docs"
    architecture_root = docs_root / "architecture"
    modules_root = architecture_root / "modules"

    for relative in sorted(RETIRED_DOC_PATHS):
        path = REPO_ROOT / relative
        if path.exists():
            errors.append(f"retired old documentation path still exists: {relative}")

    if architecture_root.is_dir():
        for path in sorted(architecture_root.rglob("*.md")):
            try:
                path.relative_to(modules_root)
            except ValueError:
                errors.append(f"architecture markdown must live under docs/architecture/modules: {_rel(path)}")

    for path in sorted(docs_root.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        if _cjk_count(text) == 0:
            errors.append(f"{_rel(path)}: markdown document must be Chinese current-state documentation")

    readme = REPO_ROOT / "README.md"
    if readme.exists():
        text = readme.read_text(encoding="utf-8")
        forbidden_refs = [
            "docs/architecture/execution" + "-map.md",
            "docs/architecture/naming" + "-and-concept-map.md",
            "docs/architecture/agent" + "-host-transition.md",
            "docs/architecture/migration" + "-to-desktop.md",
            "docs/evaluation" + "-harness.md",
            "docs/" + "adr/",
        ]
        for ref in forbidden_refs:
            if ref in text:
                errors.append(f"README.md references retired documentation path: {ref}")


def _extract_doc_id(text: str) -> str:
    prefix = "模块数据流文档 ID:"
    for line in text.splitlines():
        if line.strip().startswith(prefix):
            return line.split(":", 1)[1].strip()
    return ""


def _extract_list(text: str, marker: str) -> list[str]:
    lines = text.splitlines()
    values: list[str] = []
    in_block = False
    for line in lines:
        stripped = line.strip()
        if stripped == marker:
            in_block = True
            continue
        if in_block:
            if not stripped:
                break
            if line.startswith("- "):
                values.append(line[2:].strip().strip("`"))
                continue
            if line.startswith("#"):
                break
    return values


def _extract_field_checks(text: str) -> dict[str, list[str]]:
    checks: dict[str, list[str]] = {}
    for value in _extract_list(text, "字段清单:"):
        if ":" not in value:
            continue
        class_name, raw_fields = value.split(":", 1)
        fields = [
            field.strip().strip("`")
            for field in raw_fields.split(",")
            if field.strip()
        ]
        if fields:
            checks[class_name.strip().strip("`")] = fields
    return checks


def _extract_headings(text: str) -> set[str]:
    headings: set[str] = set()
    for line in text.splitlines():
        if not line.startswith("## "):
            continue
        heading = line[3:].strip()
        if "（" in heading:
            heading = heading.split("（", 1)[0].strip()
        if " (" in heading:
            heading = heading.split(" (", 1)[0].strip()
        headings.add(heading)
    return headings


def _extract_section(text: str, heading: str) -> str:
    lines = text.splitlines()
    start: int | None = None
    for index, line in enumerate(lines):
        if line.strip() == heading:
            start = index + 1
            break
    if start is None:
        return ""
    end = len(lines)
    for index in range(start, len(lines)):
        if lines[index].startswith("## "):
            end = index
            break
    return "\n".join(lines[start:end]).strip()


def _verify_flow_section(label: str, section: str, errors: list[str]) -> None:
    if len(section) < 120:
        errors.append(f"{label}: '真实任务中的对象流' section is too short")
    if "->" not in section:
        errors.append(f"{label}: '真实任务中的对象流' section must include a concrete call chain with '->'")
    if not re.search(r"[A-Za-z_][A-Za-z0-9_.]*\(", section):
        errors.append(f"{label}: '真实任务中的对象流' section must mention at least one concrete function or method call")
    if re.search(r"(生成|消费|写入|落盘|进入|返回|读取)[^。\n]{0,12}(对象|结果|事件|摘要|报告|store|sqlite|jsonl|artifact)", section) is None:
        errors.append(f"{label}: '真实任务中的对象流' section must describe at least one generation/consumption/persistence step")
    lowered = section.lower()
    if not any(marker in lowered for marker in REAL_TASK_FLOW_MARKERS):
        errors.append(f"{label}: '真实任务中的对象流' section must mention a concrete runtime sink or store")


def _symbols_in_sources(paths: list[Path]) -> set[str]:
    symbols: set[str] = set()
    for path in paths:
        if path.suffix != ".py":
            continue
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        except SyntaxError:
            continue
        for node in tree.body:
            if isinstance(node, ast.ClassDef):
                symbols.add(node.name)
                for child in node.body:
                    if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef):
                        symbols.add(f"{node.name}.{child.name}")
                    elif isinstance(child, ast.Assign):
                        for target in child.targets:
                            if isinstance(target, ast.Name):
                                symbols.add(f"{node.name}.{target.id}")
                    elif isinstance(child, ast.AnnAssign) and isinstance(child.target, ast.Name):
                        symbols.add(f"{node.name}.{child.target.id}")
            elif isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef):
                symbols.add(node.name)
            elif isinstance(node, ast.Assign):
                for target in node.targets:
                    if isinstance(target, ast.Name):
                        symbols.add(target.id)
    return symbols


def _class_fields_in_sources(paths: list[Path]) -> dict[str, set[str]]:
    fields_by_class: dict[str, set[str]] = {}
    for path in paths:
        if path.suffix != ".py":
            continue
        try:
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
        except SyntaxError:
            continue
        for node in tree.body:
            if not isinstance(node, ast.ClassDef):
                continue
            fields = fields_by_class.setdefault(node.name, set())
            for child in node.body:
                if not isinstance(child, ast.AnnAssign) or not isinstance(child.target, ast.Name):
                    continue
                if _is_class_var(child.annotation):
                    continue
                fields.add(child.target.id)
    return fields_by_class


def _is_class_var(annotation: ast.AST) -> bool:
    if isinstance(annotation, ast.Name):
        return annotation.id == "ClassVar"
    if isinstance(annotation, ast.Subscript):
        return _is_class_var(annotation.value)
    if isinstance(annotation, ast.Attribute):
        return annotation.attr == "ClassVar"
    return False


def _verify_forbidden_terms(errors: list[str]) -> None:
    terms = _forbidden_terms()
    for path in _iter_scan_files():
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for line_no, line in enumerate(text.splitlines(), start=1):
            for term in terms:
                if term not in line:
                    continue
                if _allowed_forbidden_hit(path, term, line):
                    continue
                errors.append(f"{_rel(path)}:{line_no}: forbidden old term remains: {term}")


def _forbidden_terms() -> list[str]:
    return [
        "LEGACY" + "_LIVE",
        "evaluation." + "live" + "_agent",
        "live" + "_agent",
        "Live" + "Eval",
        "Live" + "Agent",
        "eval" + " live",
        "deprecated compatibility" + " alias",
        "migration" + " input",
        "retired" + " live",
        "Runtime" + " Flow",
    ]


def _allowed_forbidden_hit(path: Path, term: str, line: str) -> bool:
    rel = _rel(path)
    old_schema_prefix = "evaluation." + "live" + "_agent"
    old_schema_fragment = "live" + "_agent"
    if not rel.startswith("tests/"):
        return False
    if term not in {old_schema_prefix, old_schema_fragment}:
        return False
    return "schema_version" in line and old_schema_prefix in line


def _iter_scan_files() -> list[Path]:
    roots = [
        REPO_ROOT / "AGENTS.md",
        REPO_ROOT / "docs",
        REPO_ROOT / "src",
        REPO_ROOT / "scripts",
        REPO_ROOT / "tests",
    ]
    files: list[Path] = []
    for root in roots:
        if root.is_file():
            files.append(root)
            continue
        if not root.is_dir():
            continue
        for path in root.rglob("*"):
            if not path.is_file() or path.suffix not in SCAN_SUFFIXES:
                continue
            if any(part in SKIP_DIRS for part in path.parts):
                continue
            files.append(path)
    return sorted(files)


def _rel(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def _cjk_count(text: str) -> int:
    return sum(1 for char in text if "\u4e00" <= char <= "\u9fff")


if __name__ == "__main__":
    _ensure_utf8_stdio()
    raise SystemExit(main())
