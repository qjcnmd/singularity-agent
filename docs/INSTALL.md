# Singularity 安装说明

Singularity 当前作为本地 Python CLI 运行。

## 安装

使用 uv（推荐，项目提供 `uv.lock`）：

```bash
uv sync
```

或使用 pip：

```bash
pip install -e .
```

可选依赖组：

```bash
uv sync --extra eval       # evaluation 依赖 (PyYAML)
uv sync --extra devtools   # 开发工具 (tiktoken)
```

Dependency groups（开发/测试/构建）：

```bash
uv sync --group dev        # dev 依赖 (mypy, pytest, ruff, pytest-cov, PyYAML)
uv sync --group test       # test 依赖 (pytest, pytest-cov, PyYAML)
```

安装后可用入口：

```bash
singularity-agent --help
sg --help
```

## 配置

OpenAI-compatible provider 通过环境变量配置：

```powershell
$env:SINGULARITY_BASE_URL = "https://api.openai.com/v1"
$env:SINGULARITY_API_KEY = "..."
$env:SINGULARITY_MODEL = "gpt-4.1-mini"
```

API key 只通过环境变量读取，不写入文档、trace、report 或 CLI 参数。

## 本地状态

运行时状态由 release、trace、context、memory、evaluation 和 plugin 组件管理。常用检查命令：

```bash
singularity-agent doctor --json
singularity-agent repair --dry-run --json
singularity-agent system init --json
singularity-agent system export --output singularity-export.zip --json
```

生成的 trace、evaluation、context、memory 和插件状态不属于源码文档，应保存在 `.singularity/`、`work/` 或显式输出目录中。

## 验证

```bash
python -m compileall src scripts
python -m ruff check .
python scripts/verify_runtime_docs.py
python -m pytest tests --basetemp work/pytest-tmp
```
