from __future__ import annotations

from singularity.runtime.resources import close_runtime_resources


class _KernelWithCloser:
    def __init__(self) -> None:
        self.closed = False

    def close_resources(self) -> None:
        self.closed = True


class _KernelWithoutCloser:
    closed = False


def test_close_runtime_resources_invokes_kernel_closer() -> None:
    kernel = _KernelWithCloser()

    assert close_runtime_resources(kernel) is True

    assert kernel.closed is True


def test_close_runtime_resources_ignores_missing_kernel_closer() -> None:
    assert close_runtime_resources(None) is False
    assert close_runtime_resources(_KernelWithoutCloser()) is False
