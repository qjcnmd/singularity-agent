from __future__ import annotations


class PromptAssemblyError(Exception):
    pass


class InstructionSourceError(PromptAssemblyError):
    pass


class InstructionHierarchyError(PromptAssemblyError):
    pass


class InstructionConflictError(PromptAssemblyError):
    pass


class PromptCompilationError(PromptAssemblyError):
    pass


class PromptInjectionWarning(PromptAssemblyError):
    pass


class PromptBudgetExceeded(PromptCompilationError):
    pass
