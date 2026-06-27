# 测试体系指南

## 测试分层

Singularity 测试体系按 pytest marker 分为以下层级：

| Marker | 测试数量 | 说明 |
|--------|---------|------|
| `unit` | ~626 | 纯函数/类测试，最小化跨组件依赖 |
| `integration` | ~185 | 多组件集成测试，真实子系统连线 |
| `regression` | ~67 | 生产基线、文档一致性、schema 稳定性守卫 |
| `security` | ~54 | 信任边界、脱敏、注入、密钥安全测试 |
| `evaluation` | ~58 | 评估基础设施：评分、回放、benchmark harness |
| `provider_eval` | 1 | 需真实模型 provider 的烟雾测试 |
| `slow` | ~16 | 耗时 >5s 或需要外部资源（Docker、git、网络） |

## 日常开发

```bash
# 默认快速测试（排除 evaluation、provider_eval、slow）
python -m pytest

# 只跑单元测试
python -m pytest -m unit

# 只跑某个模块的测试
python -m pytest tests/test_context.py
python -m pytest tests/test_planner.py
python -m pytest -m "unit and security"
```

## 改动某个模块后

```bash
# 改了 context 模块
python -m pytest tests/test_context*.py tests/test_context_*.py -v

# 改了 planner 模块
python -m pytest tests/test_planner.py tests/test_semantic_planner*.py tests/test_task_controller.py -v

# 改了 policy 模块
python -m pytest tests/test_policy*.py tests/test_approval_gate.py tests/test_security_regression.py -v

# 改了 tool_executor 模块
python -m pytest tests/test_tool_executor*.py tests/test_tools.py -v

# 改了 verification 模块
python -m pytest tests/test_verification_runner.py tests/test_repair_contract_verification.py -v

# 改了 sandbox 模块
python -m pytest tests/test_sandbox*.py -v

# 改了 model 模块
python -m pytest tests/test_model*.py -v

# 改了 evaluation 模块
python -m pytest tests/evaluation/ -v
```

## 提交前

```bash
# 全量快速测试（默认路径，排除 evaluation/provider_eval/slow）
python -m pytest --tb=short -q

# 带覆盖率
python -m pytest --tb=short -q --cov=singularity --cov-report=term-missing
```

## Release 前

```bash
# 1. 全量测试（包含 slow）
python -m pytest -m "not evaluation and not provider_eval" --tb=short -q

# 2. 安全测试单独确认
python -m pytest -m security --tb=short -v

# 3. 回归测试单独确认
python -m pytest -m regression --tb=short -v

# 4. 评估基础设施测试
python -m pytest -m evaluation --tb=short -v

# 5. 真实 provider 测试（需要配置 .env）
SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval --tb=short -v

# 6. 真实 evaluation benchmark
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id release-smoke --json
```

## 测试命令速查

| 场景 | 命令 |
|------|------|
| 日常开发 | `python -m pytest` |
| 单元测试 | `python -m pytest -m unit` |
| 集成测试 | `python -m pytest -m integration` |
| 安全测试 | `python -m pytest -m security` |
| 回归测试 | `python -m pytest -m regression` |
| 评估测试 | `python -m pytest -m evaluation` |
| 慢测试 | `python -m pytest -m slow` |
| 全量（含慢） | `python -m pytest -m "not evaluation and not provider_eval"` |
| 全量（含评估） | `python -m pytest -m "not provider_eval"` |
| 真实 provider | `SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval` |
| 完整（所有） | `python -m pytest -m ""` |

## 测试文件组织

```
tests/
├── conftest.py                    # 自动 marker 应用 + 策略隔离 fixture
├── agent_loop_helpers.py          # AgentLoop 测试辅助
├── tool_executor_helpers.py       # ToolExecutor 测试辅助
├── test_agent*.py                 # Agent loop 核心链路
├── test_context*.py               # Context 管理
├── test_model*.py                 # Model runner/provider
├── test_planner*.py               # Planner 引擎
├── test_policy*.py                # Policy/approval
├── test_tool_executor*.py         # Tool 执行
├── test_tool_protocol*.py         # Tool protocol 引擎
├── test_verification_runner.py    # Verification 链路
├── test_repair_contract_verification.py  # Repair 链路
├── test_security_regression.py    # 安全回归守卫
├── test_sandbox*.py               # Sandbox 隔离
├── test_trace*.py                 # Observability/trace
├── test_workspace*.py             # Workspace 状态
├── code_index/                    # Code index 子系统
├── diagnostics/                   # Doctor/repair CLI
├── edit/                          # Edit executor
├── evaluation/                    # 评估基础设施
├── interaction/                   # Interaction controller
├── memory/                        # Memory 子系统
├── plugins/                       # Plugin 管理
└── review/                        # Review pipeline
```

## 关键保护边界

以下测试 **不允许删除**，它们保护生产信任边界：

- `test_security_regression.py` — 安全攻击场景回归
- `test_approval_gate.py` — 审批门控
- `test_context_redaction.py` — 敏感信息脱敏
- `test_tool_executor_redaction.py` — 工具执行脱敏
- `test_tool_executor_secret_safety.py` — .env/密钥保护
- `test_prompt_injection_detector.py` — 提示词注入检测
- `test_sandbox_*.py` — Sandbox 隔离
- `test_trace_redaction.py` — Trace 脱敏
- `test_verification_runner.py` — Verification 链路
- `test_repair_contract_verification.py` — Repair 链路
- `test_agent_task_outcome.py` — Agent 主链路
- `test_context_production.py` — 生产级 context 组装
- `test_context_store_production.py` — 生产级 store
- `test_context_recovery_production.py` — 生产级恢复
- `test_tool_registry_production.py` — 生产级工具注册
