from __future__ import annotations


def test_runtime_defaults_define_shared_limits() -> None:
    from singularity.runtime import defaults

    assert defaults.SQLITE_BUSY_TIMEOUT_MS == 5000
    assert defaults.ATOMIC_WRITE_REPLACE_RETRY_ATTEMPTS == 8
    assert defaults.ATOMIC_WRITE_REPLACE_RETRY_BASE_SECONDS == 0.05
    assert defaults.DEFAULT_TOOL_EXECUTION_TIMEOUT_SECONDS == 120.0
    assert defaults.DEFAULT_REGISTERED_TOOL_TIMEOUT_SECONDS == 5.0
    assert defaults.DEFAULT_TOOL_MAX_OUTPUT_CHARS == 20000
    assert defaults.DEFAULT_TOOL_CACHE_MAX_ENTRIES == 128
    assert defaults.EVALUATION_PREPARE_TIMEOUT_SECONDS == 120.0
    assert defaults.BENCHMARK_VERIFICATION_TIMEOUT_SECONDS == 120.0
    assert int(
        defaults.BENCHMARK_VERIFICATION_TIMEOUT_SECONDS
    ) == defaults.EVALUATION_TASK_VERIFICATION_TIMEOUT_SECONDS
    assert defaults.WINDOWS_RUNNER_DEFAULT_TIMEOUT_SECONDS == 120.0
    assert defaults.WINDOWS_SANDBOX_ACL_COMMAND_TIMEOUT_SECONDS == 120.0
    assert defaults.SANDBOX_ISOLATED_VERIFICATION_TIMEOUT_SECONDS == 120
    assert defaults.SANDBOX_VERIFICATION_MAX_ARTIFACT_BYTES == 20 * 1024 * 1024
    assert defaults.WORKSPACE_MEDIUM_DIFF_LINE_THRESHOLD == 100
    assert defaults.CONTEXT_SUMMARY_MAX_TOKENS == 160
    assert defaults.PYTHON_RUFF_TIMEOUT_SECONDS == 120
    assert defaults.CAPABILITY_SLA_REQUIRED_THRESHOLDS_SECONDS["wall"] == 300.0
    assert defaults.CAPABILITY_SLA_OPTIONAL_THRESHOLDS_SECONDS["verification"] == 10.0
