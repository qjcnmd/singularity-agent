from __future__ import annotations


class ModelRuntimeError(RuntimeError):
    pass


class ModelProviderError(ModelRuntimeError):
    pass


class ModelProviderNotFound(ModelRuntimeError):
    pass


class ModelCapabilityError(ModelRuntimeError):
    pass


class ModelRequestValidationError(ModelRuntimeError):
    pass


class ModelResponseValidationError(ModelRuntimeError):
    pass


class ModelContextTooLong(ModelRuntimeError):
    pass


class ModelBudgetExceeded(ModelRuntimeError):
    pass


class ModelToolCallParseError(ModelRuntimeError):
    pass


class ModelRetryExhausted(ModelRuntimeError):
    pass

