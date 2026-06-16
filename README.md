# Miniharness v0.0.3

Miniharness is a tiny read-only CLI coding agent harness. It is intentionally small so you can see how the agent loop, provider, tools, and trace file connect without using LangChain, LangGraph, or any other agent framework.

## Project Structure

```txt
.
├── pyproject.toml
├── README.md
├── tests/
└── src/
    └── miniharness/
        ├── __init__.py
        ├── agent.py       # agent loop
        ├── cli.py         # Typer command entry
        ├── config.py      # environment variable loading
        ├── context/       # context manager and tool observations
        ├── provider.py    # OpenAI-compatible HTTP call via httpx
        ├── tools.py       # read-only tools and Pydantic schemas
        └── trace.py       # JSONL trace writer
```

Each run creates:

```txt
.miniharness/runs/<run_id>.jsonl
```

The trace records `user_goal`, `model_request`, `model_response`, `tool_call`, `tool_result`, `final_answer`, and `error` events.

## Install

From this project directory:

```powershell
python -m pip install -e .
```

On Windows, if pip says the script directory is not on `PATH`, you can enable it for the current PowerShell session:

```powershell
$env:PATH = "$env:APPDATA\Python\Python313\Scripts;$env:PATH"
```

Or run the module form without changing `PATH`:

```powershell
python -m miniharness.cli "请阅读 README 并总结这个项目"
```

## Configure Environment Variables

Miniharness calls an OpenAI-compatible Chat Completions API. The base URL should usually include `/v1`.

PowerShell example:

```powershell
$env:MINIHARNESS_BASE_URL = "https://api.openai.com/v1"
$env:MINIHARNESS_API_KEY = "sk-..."
$env:MINIHARNESS_MODEL = "gpt-4.1-mini"
```

For a local OpenAI-compatible server:

```powershell
$env:MINIHARNESS_BASE_URL = "http://localhost:8000/v1"
$env:MINIHARNESS_API_KEY = "local-key"
$env:MINIHARNESS_MODEL = "your-model"
```

The `.env` file is only loaded automatically by VSCode's `launch.json` debug configuration. A normal terminal session does not read `.env` by itself, so set the variables manually as shown above, or load them into the shell before running `miniharness`.

## Run

```powershell
miniharness "请阅读 README 并总结这个项目"
```

You can cap the loop:

```powershell
miniharness "找一下 agent loop 在哪里" --max-turns 6
```

## Test

Install the development dependency:

```powershell
python -m pip install -e ".[dev]"
```

Run the tests:

```powershell
python -m pytest tests --basetemp work/pytest-tmp
```

The tests use temporary files and a mock provider. They do not call a live model API and do not require `.env`.

## Agent Loop Flow

1. `cli.py` receives the user goal and creates a trace file.
2. `agent.py` creates a `ContextManager` with the system message and user goal.
3. `provider.py` sends the context-managed `messages` and tool schemas to the OpenAI-compatible API.
4. If the model returns no tool calls, the assistant message is the final answer.
5. If the model returns tool calls, `agent.py` dispatches them through `tools.py`.
6. Each tool result is recorded as a `ToolObservation`; a preview is appended back into `messages` as a `tool` role message.
7. The loop calls the model again with the updated `messages`.
8. The loop stops when the model gives a final answer or `--max-turns` is reached.

## Context Manager

Miniharness v0.0.3 moves message ownership out of the agent loop and into `ContextManager`.

The context layer now:

- Initializes the system and user messages.
- Records assistant messages.
- Records tool observations with raw results, previews, truncation status, and small metadata.
- Sends only the first 4000 characters of long tool content back into model messages while keeping the full raw result in memory for traceable local inspection.

## Tool Calling Protocol

Miniharness v0.0.3 keeps the existing default CLI behavior: tool choice is sent as `auto`, and strict tool schemas are disabled unless a caller explicitly enables them.

The protocol layer now has:

- `ToolChoiceMode.AUTO`: the model may call tools or answer directly.
- `ToolChoiceMode.REQUIRED`: the model must call at least one tool, for providers that support this mode.
- `ToolChoiceMode.NONE`: the model must answer without tool calls.
- `ProviderCapabilities`: a small capability record for OpenAI-compatible providers, including support flags for tools, strict schemas, required tool choice, and parallel tool calls.
- `ToolRegistry.openai_tools(strict=True)`: emits `strict: true` function schemas and top-level `additionalProperties: false` parameters while still validating tool arguments locally with Pydantic.

## Read-Only Tools

Miniharness v0.0.3 only exposes these tools:

- `list_files`: list files under the current project root.
- `read_file`: read a file inside the current project root.
- `search_text`: search text inside files under the current project root.

The tools cannot write files, run shell commands, run Git commands, browse the web, store long-term memory, or start other agents. Paths are resolved inside the current project root, so `../outside-file` is rejected.

## VSCode Setup

This project can run inside a project-local virtual environment:

```powershell
python -m venv .venv
.\.venv\Scripts\python.exe -m pip install -e .
```

VSCode settings are in `.vscode/`:

- `settings.json` points Python to `.venv\Scripts\python.exe`.
- `launch.json` defines `Miniharness: run sample goal`.
- `tasks.json` defines `Miniharness: help` and `Miniharness: compile`.

Put local API settings in `.env`. This file is ignored by Git:

```txt
MINIHARNESS_BASE_URL=https://example.com/v1
MINIHARNESS_API_KEY=replace-with-your-api-key
MINIHARNESS_MODEL=your-model-name
```
