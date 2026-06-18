from __future__ import annotations


class ToolProtocolError(RuntimeError):
    pass


class ToolProtocolValidationError(ToolProtocolError):
    pass


class ToolProtocolStateError(ToolProtocolError):
    pass


class ToolProtocolRecoveryError(ToolProtocolError):
    pass
