from __future__ import annotations


class ModelRunnerError(RuntimeError):
    pass


class ModelProviderError(ModelRunnerError):
    pass


class ModelProviderNotFound(ModelRunnerError):
    pass


class ModelCapabilityError(ModelRunnerError):
    pass


class ModelRequestValidationError(ModelRunnerError):
    pass


class ModelResponseValidationError(ModelRunnerError):
    pass


class ModelContextTooLong(ModelRunnerError):
    pass


class ModelBudgetExceeded(ModelRunnerError):
    pass


class ModelToolCallParseError(ModelRunnerError):
    pass


class ModelRetryExhausted(ModelRunnerError):
    pass

