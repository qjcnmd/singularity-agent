from __future__ import annotations

from singularity.kernel.models import ShutdownReason
from singularity.kernel.shutdown import ShutdownManager


class Component:
    def __init__(self, fail: bool = False) -> None:
        self.called: list[str] = []
        self.fail = fail

    def stop(self) -> None:
        self.called.append("stop")
        if self.fail:
            raise RuntimeError("stop failed")


class Lock:
    def __init__(self) -> None:
        self.released = False

    def release_lock(self) -> None:
        self.released = True


def test_shutdown_manager_continues_cleanup_after_failure() -> None:
    planner = Component(fail=True)
    model = Component()
    lock = Lock()
    manager = ShutdownManager(
        planner=planner,
        model=model,
        workspace_lock=lock,
    )

    summary = manager.shutdown(ShutdownReason.ERROR)

    assert summary.reason == ShutdownReason.ERROR
    assert summary.cleanup_status == "completed_with_errors"
    assert summary.steps[0]["status"] == "failed"
    assert model.called == ["stop"]
    assert lock.released is True
