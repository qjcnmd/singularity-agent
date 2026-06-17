import json
from pathlib import Path

from miniharness.tools import ToolPolicy, ToolRegistry, ToolRuntime
from miniharness.trace import TraceWriter
from tests.tool_runtime_helpers import make_test_policy_runtime


def make_tool_call(name: str, arguments: dict) -> dict:
    return {
        "id": f"call_{name}",
        "type": "function",
        "function": {"name": name, "arguments": json.dumps(arguments)},
    }


def make_runtime(tmp_path: Path) -> ToolRuntime:
    return ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=TraceWriter.create(tmp_path),
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )


def test_read_file_sensitive_env_is_denied_or_requires_review(tmp_path: Path) -> None:
    (tmp_path / ".env").write_text("OPENAI_API_KEY=sk-secret\n", encoding="utf-8")

    result = make_runtime(tmp_path).execute_tool_call(
        make_tool_call("read_file", {"path": ".env"})
    )

    assert result.ok is False
    assert result.error_code in {"policy_denied", "approval_required", "sensitive_path_denied"}
    assert "sk-secret" not in str(result.model_dump(mode="json"))
    assert ".env" not in str(result.model_dump(mode="json"))


def test_sensitive_path_is_redacted_from_tool_and_policy_trace(tmp_path: Path) -> None:
    (tmp_path / ".env").write_text("OPENAI_API_KEY=sk-secret\n", encoding="utf-8")
    trace = TraceWriter.create(tmp_path)
    runtime = ToolRuntime(
        registry=ToolRegistry(tmp_path),
        policy=ToolPolicy.read_only(),
        trace=trace,
        workspace_root=tmp_path,
        policy_runtime=make_test_policy_runtime(tmp_path),
    )

    result = runtime.execute_tool_call(make_tool_call("read_file", {"path": ".env"}))

    assert result.ok is False
    trace_text = trace.path.read_text(encoding="utf-8")
    assert ".env" not in trace_text
    assert "sk-secret" not in trace_text


def test_search_text_does_not_leak_env_content_from_directory_search(tmp_path: Path) -> None:
    (tmp_path / ".env").write_text("OPENAI_API_KEY=sk-secret\n", encoding="utf-8")
    (tmp_path / "app.py").write_text("API_KEY_NAME = 'safe label'\n", encoding="utf-8")

    result = make_runtime(tmp_path).execute_tool_call(
        make_tool_call("search_text", {"query": "API_KEY", "path": "."})
    )

    assert result.ok is True
    dumped = json.dumps(result.model_dump(mode="json"), ensure_ascii=False)
    assert "sk-secret" not in dumped
    assert ".env" not in dumped
    assert "app.py" in dumped


def test_list_files_hides_sensitive_paths_by_default(tmp_path: Path) -> None:
    (tmp_path / ".env").write_text("TOKEN=secret-token\n", encoding="utf-8")
    (tmp_path / "README.md").write_text("hello\n", encoding="utf-8")

    result = make_runtime(tmp_path).execute_tool_call(
        make_tool_call("list_files", {"path": ".", "max_depth": 1})
    )

    assert result.ok is True
    files = result.content["files"]
    assert "README.md" in files
    assert ".env" not in files
    assert result.content["sensitive_hidden_count"] == 1


def test_secret_like_search_lines_are_redacted(tmp_path: Path) -> None:
    (tmp_path / "config.example").write_text(
        "OPENAI_API_KEY=sk-example-value\nnormal=value\n",
        encoding="utf-8",
    )

    result = make_runtime(tmp_path).execute_tool_call(
        make_tool_call("search_text", {"query": "OPENAI_API_KEY", "path": "config.example"})
    )

    assert result.ok is True
    assert result.content["matches"][0]["text"] == "OPENAI_API_KEY=<redacted>"
    assert "sk-example-value" not in str(result.model_dump(mode="json"))


def test_directory_cache_fingerprint_does_not_touch_sensitive_files(tmp_path: Path) -> None:
    secret = tmp_path / ".env"
    secret.write_text("OPENAI_API_KEY=sk-secret\n", encoding="utf-8")
    secret.chmod(0)
    try:
        result = make_runtime(tmp_path).execute_tool_call(
            make_tool_call("list_files", {"path": ".", "max_depth": 1})
        )
    finally:
        secret.chmod(0o600)

    assert result.ok is True
    assert ".env" not in result.content["files"]

