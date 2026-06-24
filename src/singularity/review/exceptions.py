from __future__ import annotations


class ReviewPipelineError(RuntimeError):
    def __init__(self, message: str, *, code: str = "review_pipeline_error") -> None:
        super().__init__(message)
        self.code = code


class ReviewCriticError(ReviewPipelineError):
    pass
