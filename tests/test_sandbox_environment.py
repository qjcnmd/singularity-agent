from singularity.sandbox import SandboxEnvironmentBuilder, SandboxEnvPolicy


def test_default_env_does_not_leak_api_key() -> None:
    builder = SandboxEnvironmentBuilder()
    env = builder.build_env(
        SandboxEnvPolicy(extra_env={"VISIBLE": "ok"}),
        {"OPENAI_API_KEY": "sk-secret", "PATH": "C:\\bin"},
    )

    assert "OPENAI_API_KEY" not in env
    assert env["VISIBLE"] == "ok"


def test_inherit_env_still_filters_and_redacts_sensitive_values() -> None:
    builder = SandboxEnvironmentBuilder()
    env = builder.build_env(
        SandboxEnvPolicy(inherit_env=True),
        {"GITHUB_TOKEN": "gh-secret", "SAFE_VALUE": "visible"},
    )
    redacted = builder.redact_env({"GITHUB_TOKEN": "gh-secret", "SAFE_VALUE": "visible"})

    assert "GITHUB_TOKEN" not in env
    assert env["SAFE_VALUE"] == "visible"
    assert redacted["GITHUB_TOKEN"] == "[REDACTED]"
    assert redacted["SAFE_VALUE"] == "visible"


def test_case_insensitive_secret_env_key_is_filtered() -> None:
    builder = SandboxEnvironmentBuilder()
    env = builder.build_env(
        SandboxEnvPolicy(inherit_env=True, case_insensitive=True),
        {"openai_api_key": "sk-secret", "Path": "C:\\bin"},
    )

    assert "openai_api_key" not in env
    assert env["Path"] == "C:\\bin"


def test_extra_env_can_inject_non_sensitive_values() -> None:
    builder = SandboxEnvironmentBuilder()
    env = builder.build_env(
        SandboxEnvPolicy(extra_env={"SINGULARITY_SANDBOX": "1"}),
        {},
    )

    assert env["SINGULARITY_SANDBOX"] == "1"
