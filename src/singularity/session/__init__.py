from singularity.session.models import (
    RecoveryGateDecision,
    RecoveryGateStatus,
    SessionCheckpoint,
    SessionCheckpointKind,
    SessionDetail,
    SessionLaunch,
    SessionResumeContext,
    SessionRun,
    SessionRunMode,
    SessionState,
    SessionStatus,
    SessionSummary,
    SessionTimelineEvent,
)

__all__ = [
    "RecoveryGateDecision",
    "RecoveryGateStatus",
    "SessionCheckpoint",
    "SessionCheckpointKind",
    "SessionDetail",
    "SessionHistoryReader",
    "SessionLaunch",
    "SessionRecoveryGate",
    "SessionResumeContext",
    "SessionRun",
    "SessionRunMode",
    "SessionState",
    "SessionStatus",
    "SessionStore",
    "SessionSummary",
    "SessionTimelineEvent",
]


def __getattr__(name: str):
    if name == "SessionHistoryReader":
        from singularity.session.history import SessionHistoryReader

        return SessionHistoryReader
    if name == "SessionRecoveryGate":
        from singularity.session.recovery import SessionRecoveryGate

        return SessionRecoveryGate
    if name == "SessionStore":
        from singularity.session.store import SessionStore

        return SessionStore
    raise AttributeError(name)
