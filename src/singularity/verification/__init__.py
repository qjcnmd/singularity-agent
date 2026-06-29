from singularity.verification.assessor import CompletionAssessor
from singularity.verification.discovery import CommandDiscovery, ProjectDetector
from singularity.verification.errors import VERIFICATION_ERROR_CODES
from singularity.verification.impact import ImpactAnalyzer
from singularity.verification.models import (
    CheckKind,
    CheckStatus,
    CompletionAssessment,
    CompletionStatus,
    DiscoveredCommand,
    FailureType,
    ImpactAnalysis,
    ParsedFailure,
    ProjectLanguage,
    ProjectProfile,
    RepairBudget,
    RepairHint,
    RepairLoopState,
    VerificationCheck,
    VerificationDecision,
    VerificationEvidence,
    VerificationPlan,
    VerificationResult,
    WorkspaceKind,
)
from singularity.verification.parsers import (
    FailureParser,
    FailureParserRegistry,
    classify_failure,
)
from singularity.verification.policy import VerificationPolicy, VerificationPolicyResult
from singularity.verification.repair import RepairHintGenerator, RepairLoopController

__all__ = [
    "VERIFICATION_ERROR_CODES",
    "CheckKind",
    "CheckStatus",
    "CommandDiscovery",
    "CompletionAssessment",
    "CompletionAssessor",
    "CompletionStatus",
    "DiscoveredCommand",
    "FailureAnalysisPipeline",
    "FailureParser",
    "FailureParserRegistry",
    "FailureType",
    "ImpactAnalysis",
    "ImpactAnalyzer",
    "NoProgressGuard",
    "ParsedFailure",
    "ProjectDetector",
    "ProjectLanguage",
    "ProjectProfile",
    "RepairBudget",
    "RepairHint",
    "RepairHintGenerator",
    "RepairLoopController",
    "RepairLoopState",
    "VerificationCheck",
    "VerificationDecision",
    "VerificationEvidence",
    "VerificationPlan",
    "VerificationPolicy",
    "VerificationPolicyResult",
    "VerificationResult",
    "VerificationRunner",
    "WorkspaceKind",
    "classify_failure",
]


def __getattr__(name: str) -> object:
    if name in {"FailureAnalysisPipeline", "NoProgressGuard"}:
        from singularity.verification.failure_analysis import (
            FailureAnalysisPipeline,
            NoProgressGuard,
        )

        exports = {
            "FailureAnalysisPipeline": FailureAnalysisPipeline,
            "NoProgressGuard": NoProgressGuard,
        }
        return exports[name]
    if name == "VerificationRunner":
        from singularity.verification.runner import VerificationRunner

        return VerificationRunner
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
