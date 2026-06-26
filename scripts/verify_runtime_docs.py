from __future__ import annotations

import ast
import re
import sys
from dataclasses import dataclass
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[1]
DOCS_DIR = REPO_ROOT / "docs" / "architecture" / "modules"

CORE_DOC_IDS = {
    "tool-registry-exposure",
    "plugin-tools-registry",
    "tool-execution-runtime",
    "model-turn-provider-tools",
    "policy-approval-gates",
    "context-assembly-prompt-frame",
    "context-compaction-observation-store",
    "planner-replanner-failure-recovery",
    "evaluation-benchmark-runner",
    "trace-observation-audit-events",
    "artifact-long-result-handling",
}

REQUIRED_HEADINGS = {
    "Module Boundary",
    "Current Source Locations",
    "Runtime Call Chain",
    "Runtime Objects Passed",
    "Model-Visible Objects",
    "Internal Trace Debug Audit Objects",
    "State Transitions And Failure Paths",
    "Current Structure Assessment",
    "Production-Grade Target Structure",
    "Harness Usage Example",
    "Maintenance Rules",
    "Verification",
    "Last Verified Against",
}

REQUIRED_PHRASES = {
    "Model-Visible Objects",
    "Internal Trace Debug Audit Objects",
}

COMPLETE_FIELD_CHECKS = {
    "evaluation-benchmark-runner": {
        "EvaluationWorkspace",
        "EvaluationTask",
        "EvaluationTaskSet",
        "CommandEvalResult",
        "EvaluationTaskResult",
        "TargetedFailureReplayResult",
    },
    "planner-replanner-failure-recovery": {
        "FailureAnalysisRequest",
        "FailureAnalysisResult",
        "RepairContract",
        "RepairPlan",
        "RepairReplanSignal",
        "VerificationContract",
        "VerificationStep",
        "ContractSatisfaction",
        "StepEvidence",
    },
}


@dataclass(frozen=True)
class RuntimeDoc:
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
        errors.append(f"missing core runtime doc id: {doc_id}")

    duplicate_ids = sorted(
        doc_id for doc_id in seen_ids if sum(1 for doc in docs if doc.doc_id == doc_id) > 1
    )
    for doc_id in duplicate_ids:
        errors.append(f"duplicate runtime doc id: {doc_id}")

    for doc in docs:
        _verify_doc(doc, errors)

    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1

    print(f"Runtime docs verified: {len(docs)} documents, {len(CORE_DOC_IDS)} core modules covered.")
    return 0


def _load_docs(errors: list[str]) -> list[RuntimeDoc]:
    if not DOCS_DIR.is_dir():
        errors.append(f"runtime docs directory missing: {_rel(DOCS_DIR)}")
        return []

    docs: list[RuntimeDoc] = []
    for path in sorted(DOCS_DIR.glob("*.md")):
        text = path.read_text(encoding="utf-8")
        doc_id = _extract_doc_id(text)
        docs.append(
            RuntimeDoc(
                path=path,
                text=text,
                doc_id=doc_id,
                source_paths=_extract_list(text, "Source paths:"),
                symbols=_extract_list(text, "Symbols:"),
                field_checks=_extract_field_checks(text),
                headings=_extract_headings(text),
            )
        )
    return docs


def _verify_doc(doc: RuntimeDoc, errors: list[str]) -> None:
    label = _rel(doc.path)
    if not doc.doc_id:
        errors.append(f"{label}: missing 'Runtime flow doc id:'")

    missing_headings = sorted(REQUIRED_HEADINGS - doc.headings)
    for heading in missing_headings:
        errors.append(f"{label}: missing heading '## {heading}'")

    for phrase in REQUIRED_PHRASES:
        if phrase not in doc.text:
            errors.append(f"{label}: missing required phrase '{phrase}'")

    if not doc.source_paths:
        errors.append(f"{label}: no Source paths entries")
    if not doc.symbols:
        errors.append(f"{label}: no Symbols entries")

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

    if doc.field_checks:
        class_fields = _class_fields_in_sources(existing_sources)
        for class_name, fields in doc.field_checks.items():
            if class_name not in class_fields:
                errors.append(f"{label}: field check class not found in listed source paths: {class_name}")
                continue
            for field_name in fields:
                if field_name not in class_fields[class_name]:
                    errors.append(f"{label}: field not found on {class_name}: {field_name}")

        complete_classes = COMPLETE_FIELD_CHECKS.get(doc.doc_id, set())
        for class_name in sorted(complete_classes):
            if class_name not in class_fields:
                errors.append(f"{label}: complete field check class not found in listed source paths: {class_name}")
                continue
            documented_fields = set(doc.field_checks.get(class_name, []))
            if not documented_fields:
                errors.append(f"{label}: complete field check missing Field checks entry: {class_name}")
                continue
            missing_fields = sorted(class_fields[class_name] - documented_fields)
            if missing_fields:
                errors.append(
                    f"{label}: Field checks for {class_name} missing current fields: "
                    + ", ".join(missing_fields)
                )


def _extract_doc_id(text: str) -> str:
    match = re.search(r"^Runtime flow doc id:\s*([A-Za-z0-9_.-]+)\s*$", text, re.MULTILINE)
    return match.group(1) if match else ""


def _extract_list(text: str, marker: str) -> list[str]:
    lines = text.splitlines()
    values: list[str] = []
    in_block = False
    for line in lines:
        if line.strip() == marker:
            in_block = True
            continue
        if in_block:
            if not line.strip():
                break
            if line.startswith("- "):
                values.append(line[2:].strip().strip("`"))
                continue
            if line.startswith("#"):
                break
    return values


def _extract_field_checks(text: str) -> dict[str, list[str]]:
    checks: dict[str, list[str]] = {}
    for value in _extract_list(text, "Field checks:"):
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
    for match in re.finditer(r"^##\s+(.+?)\s*$", text, re.MULTILINE):
        heading = match.group(1).strip()
        if " (" in heading:
            heading = heading.split(" (", 1)[0].strip()
        headings.add(heading)
    return headings


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
                    if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        symbols.add(f"{node.name}.{child.name}")
                    elif isinstance(child, ast.Assign):
                        for target in child.targets:
                            if isinstance(target, ast.Name):
                                symbols.add(f"{node.name}.{target.id}")
            elif isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                symbols.add(node.name)
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
                if isinstance(child, ast.AnnAssign) and isinstance(child.target, ast.Name):
                    fields.add(child.target.id)
    return fields_by_class


def _rel(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return path.as_posix()


if __name__ == "__main__":
    raise SystemExit(main())
