# Miniharness v0.0.1

Miniharness is a tiny read-only CLI coding agent harness. It is intentionally small so you can see how the agent loop, provider, tools, and trace file connect without using LangChain, LangGraph, or any other agent framework.

## Project Structure

```txt
.
├── pyproject.toml
├── README.md
└── src/
    └── miniharness/
        ├── __init__.py
        ├── agent.py       # agent loop
        ├── cli.py         # Typer command entry
        ├── config.py      # environment variable loading
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

## Run

```powershell
miniharness "请阅读 README 并总结这个项目"
```

You can cap the loop:

```powershell
miniharness "找一下 agent loop 在哪里" --max-turns 6
```

## Agent Loop Flow

1. `cli.py` receives the user goal and creates a trace file.
2. `agent.py` builds the initial `messages` list with a system message and user message.
3. `provider.py` sends `messages` and tool schemas to the OpenAI-compatible API.
4. If the model returns no tool calls, the assistant message is the final answer.
5. If the model returns tool calls, `agent.py` dispatches them through `tools.py`.
6. Each tool result is appended back into `messages` as a `tool` role message.
7. The loop calls the model again with the updated `messages`.
8. The loop stops when the model gives a final answer or `--max-turns` is reached.

## Read-Only Tools

Miniharness v0.0.1 only exposes these tools:

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
