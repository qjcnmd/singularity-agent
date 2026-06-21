from __future__ import annotations


class ReviewRuntimeError(RuntimeError):
    def __init__(self, message: str, *, code: str = "review_runtime_error") -> None:
        super().__init__(message)
        self.code = code


class ReviewCriticError(ReviewRuntimeError):
    pass
