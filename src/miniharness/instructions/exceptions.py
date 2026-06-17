from __future__ import annotations


class InstructionRuntimeError(Exception):
    pass


class InstructionSourceError(InstructionRuntimeError):
    pass


class InstructionHierarchyError(InstructionRuntimeError):
    pass


class InstructionConflictError(InstructionRuntimeError):
    pass


class PromptCompilationError(InstructionRuntimeError):
    pass


class PromptInjectionWarning(InstructionRuntimeError):
    pass


class PromptBudgetExceeded(PromptCompilationError):
    pass
