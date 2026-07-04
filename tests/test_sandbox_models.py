from pathlib import Path

from singularity.sandbox import (
    SandboxArtifact,
    SandboxCapabilities,
    SandboxFilesystemMode,
    SandboxProfileName,
    SandboxRequest,
    SandboxResult,
    SandboxStatus,
    default_sandbox_profile,
)


def test_sandbox_models_construct_and_serialize(tmp_path: Path) -> None:
    profile = default_sandbox_profile(
        SandboxProfileName.ISOLATED_VERIFICATION,
        workspace_root=tmp_path,
    )
    request = SandboxRequest(
        sandbox_id="sandbox_1",
        session_id="session_1",
        task_id="task_1",
        action_id="action_1",
        command=["python", "-c", "print('ok')"],
        cwd=tmp_path,
        workspace_root=tmp_path,
        profile=profile,
        policy_decision_id="policy_1",
        reason="test sandbox",
    )
    artifact = SandboxArtifact(
        artifact_id="artifact_1",
        sandbox_id="sandbox_1",
        path=tmp_path / "artifact.log",
        relative_path="artifact.log",
        size_bytes=2,
        kind="log",
        sha256="digest",
    )
    result = SandboxResult(
        sandbox_id="sandbox_1",
        backend_name="windows_elevated",
        status=SandboxStatus.SUCCESS,
        exit_code=0,
        stdout="ok\n",
        stderr="",
        started_at="start",
        ended_at="end",
        duration_ms=3,
        artifacts=[artifact],
        trace_id="trace_1",
        cleanup_status="cleaned",
    )

    capabilities = SandboxCapabilities(
        filesystem_isolation=True,
        copy_on_write=True,
        readonly_mount=False,
        network_isolation=False,
        env_isolation=True,
        process_tree_kill=True,
        timeout=True,
        output_limit=True,
        memory_limit=False,
        process_limit=False,
        artifact_capture=True,
        change_detection=True,
    )

    assert request.to_dict()["profile"]["name"] == "isolated_verification"
    assert request.profile.filesystem.mode == SandboxFilesystemMode.COPY_ON_WRITE_WORKSPACE
    assert result.to_dict()["artifacts"][0]["kind"] == "log"
    assert capabilities.to_dict()["network_isolation"] is False
