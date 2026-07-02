from pathlib import Path


def test_phase88_terminology_uses_standard_output_boundary_terms() -> None:
    paths = [
        Path("src/singularity/review/critic.py"),
        Path("src/singularity/review/structured_output.py"),
        Path("src/singularity/planner/final_reviewer.py"),
        Path("docs/architecture/modules/model-turn-provider-tools.md"),
        Path("docs/architecture/modules/evaluation-benchmark-runner.md"),
        Path("docs/testing.md"),
    ]
    text = "\n".join(path.read_text(encoding="utf-8") for path in paths if path.exists())
    lowered = text.lower()

    for banned in (
        "semantic invalid",
        "repair retry",
        "critic degraded",
        "valid but risky finding",
        "deterministic hard failure",
    ):
        assert banned not in lowered

    for required in (
        "Structured Outputs",
        "tool calling",
        "tool choice",
        "JSON Schema",
        "schema validation",
        "bounded retry",
        "exponential backoff with jitter",
        "fallback path",
        "graceful degradation",
        "fail-closed",
        "deterministic gate",
        "hard gate",
        "model-assisted review",
    ):
        assert required in text

    for internal_name in (
        "ReviewPipeline",
        "ModelCritic",
        "ReviewDecisionEngine",
        "FinalReviewer",
        "CompletionGate",
        "EvidenceLedger",
        "ReviewFinding",
        "ReviewReport",
    ):
        assert internal_name in text
