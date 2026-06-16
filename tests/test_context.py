from miniharness.context import ContextManager


def test_context_manager_initializes_system_and_user_messages() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")

    assert context.messages() == [
        {"role": "system", "content": "system rules"},
        {"role": "user", "content": "inspect project"},
    ]


def test_add_assistant_message_includes_assistant_message() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    assistant_message = {
        "role": "assistant",
        "content": None,
        "tool_calls": [
            {
                "id": "call_readme",
                "type": "function",
                "function": {"name": "read_file", "arguments": "{}"},
            }
        ],
    }

    context.add_assistant_message(assistant_message)

    assert context.messages()[-1] == assistant_message


def test_add_tool_result_generates_tool_message() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    tool_call = {
        "id": "call_readme",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    result = {"ok": True, "content": "README content"}

    observation = context.add_tool_result(tool_call=tool_call, result=result)

    tool_message = context.messages()[-1]
    assert tool_message["role"] == "tool"
    assert tool_message["tool_call_id"] == "call_readme"
    assert tool_message["name"] == "read_file"
    assert '"content": "README content"' in tool_message["content"]
    assert observation.tool_name == "read_file"
    assert observation.tool_call_id == "call_readme"
    assert observation.ok is True


def test_long_tool_result_is_truncated_in_message_but_raw_result_is_preserved() -> None:
    context = ContextManager(system_prompt="system rules", user_goal="inspect project")
    tool_call = {
        "id": "call_long",
        "type": "function",
        "function": {"name": "read_file", "arguments": "{}"},
    }
    long_content = "x" * 4100
    result = {"ok": True, "content": long_content, "path": "README.md"}

    observation = context.add_tool_result(tool_call=tool_call, result=result)

    tool_message = context.messages()[-1]
    assert len(observation.preview) == 4000
    assert observation.truncated is True
    assert observation.raw_result == result
    assert '"truncated": true' in tool_message["content"]
    assert long_content not in tool_message["content"]
    assert "x" * 4000 in tool_message["content"]
