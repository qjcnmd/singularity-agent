from __future__ import annotations


class PolicyError(Exception):
    pass


class PolicyDenied(PolicyError):
    pass


class ApprovalRequired(PolicyError):
    pass


class ApprovalDenied(PolicyError):
    pass


class SandboxRequired(PolicyError):
    pass


class PolicyEscalationRequired(PolicyError):
    pass


class PolicyAskUserRequired(PolicyError):
    pass
