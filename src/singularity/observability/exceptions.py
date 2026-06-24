from __future__ import annotations


class ObservabilityError(Exception):
    """Base error for trace recording failures."""


class TraceStoreError(ObservabilityError):
    """Raised when the append-only trace store cannot be read or written."""


class TraceSerializationError(ObservabilityError):
    """Raised when trace models cannot be serialized or parsed."""


class TraceRedactionError(ObservabilityError):
    """Raised when trace redaction fails."""


class TraceArtifactError(ObservabilityError):
    """Raised when trace artifact storage fails."""


class TraceSpanError(ObservabilityError):
    """Raised when trace span state is invalid."""
