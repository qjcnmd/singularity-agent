# 测试体系指南

## 测试分层

Singularity 测试体系按 pytest marker 分为以下层级：

| Marker | 测试数 | 说明 | 默认是否运行 |
|--------|--------|------|-------------|
| `smoke` | 25 | 核心路径烟雾测试，覆盖 CLI/Context/Planner/Policy/Tool/Verification | ❌ 显式运行 |
| `unit` | ~626 | 纯函数/类测试，最小化跨组件依赖 | ✅ |
| `integration` | ~185 | 多组件集成测试，真实子系统连线 | ✅ |
| `regression` | ~68 | 生产基线、文档一致性、schema 稳定性守卫 | ✅ |
| `security` | ~54 | 信任边界、脱敏、注入、密钥安全测试 | ✅ |
| `flaky` | 4 | 已知偶发失败测试（默认仍运行，见下方处理策略） | ✅ |
| `evaluation` | ~58 | 评估基础设施：评分、回放、benchmark harness | ❌ 显式运行 |
| `slow` | 9 | 真正慢的测试（>5s），agent loop 模拟/并发 | ❌ 显式运行 |
| `external` | 16 | 依赖外部资源（Docker/git/network），实际很快 | ❌ 显式运行 |
| `provider_eval` | 1 | 需真实模型 provider 的烟雾测试 | ❌ 显式运行 |

## 日常开发

### 烟雾测试（~3 秒，25 tests）

改完代码快速验证核心路径没断：

```bash
python -m pytest -m smoke
```

覆盖：CLI 启动、Context 组装、Planner 决策、Policy 边界（12 tests）、Tool 合约（5 tests）、Approval 门控、Verification 路径、Trace 基础、Tool 协议结果。

### 按改动文件推荐测试

```bash
# 自动从 git diff 检测变更文件，推荐相关测试
python scripts/test_impact.py --git

# 指定变更文件
python scripts/test_impact.py src/singularity/context/manager.py src/singularity/planner/engine.py
```

### 按模块测试

```bash
# 改了 context 模块
python -m pytest tests/test_context*.py -v

# 改了 planner 模块
python -m pytest tests/test_planner.py tests/test_semantic_planner*.py tests/test_task_controller.py -v

# 改了 policy 模块
python -m pytest tests/test_policy*.py tests/test_approval_gate.py tests/test_security_regression.py -v

# 改了 verification 模块
python -m pytest tests/test_verification_runner.py tests/test_repair_contract_verification.py -v
```

## 提交前 Fast Gate

```bash
# 默认测试（排除 evaluation、provider_eval、slow、external）
# ~906 tests，~2.5min
python -m pytest

# 带覆盖率
python -m pytest --cov=singularity --cov-report=term-missing
```

## Release Full Gate

```bash
# 1. 全量测试（含 slow + external，排除 provider_eval）
# ~990 tests，~3.5min
python -m pytest -m "not provider_eval"

# 2. 安全测试单独确认
python -m pytest -m security -v

# 3. 回归测试单独确认
python -m pytest -m regression -v

# 4. 评估基础设施测试
python -m pytest -m evaluation -v

# 5. 真实 provider 测试（需要配置 .env）
SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval -v

# 6. 真实 evaluation benchmark
python -m singularity.cli eval run docs/evaluation/capability-regression-tasks.json --run-id release-smoke --json
```

## Flaky 测试处理策略

以下测试已知偶发失败，默认仍包含在测试运行中：

| 测试 | 原因 | 处理 |
|------|------|------|
| `test_cli.py::test_cli_eval_task_validate_and_list_filter_tags` | 偶发超时 | 失败时重跑一次确认 |
| `test_cli.py::test_cli_eval_private_uses_private_benchmark_adapter` | 偶发超时 | 失败时重跑一次确认 |
| `test_tool_executor_secret_safety.py::test_list_files_hides_sensitive_paths_by_default` | 顺序依赖 | 失败时重跑一次确认 |
| `test_observability_integration.py::test_tool_executor_dispatch_emits_structured_trace` | 顺序依赖 | 失败时重跑一次确认 |

**策略**：提交前如果 flaky 测试失败，重跑一次 `python -m pytest <test_path>` 确认。如果连续两次失败，说明是真实回归，需要修复。

**不排除 flaky 测试的原因**：这些测试保护重要路径（CLI eval、secret safety、observability），排除会降低保护覆盖率。

## 测试命令速查

| 场景 | 命令 | 测试数 | 耗时 |
|------|------|--------|------|
| 烟雾测试 | `python -m pytest -m smoke` | 25 | ~3s |
| 默认（提交前） | `python -m pytest` | 906 | ~2.5min |
| 全量（release） | `python -m pytest -m "not provider_eval"` | 990 | ~3.5min |
| 单元测试 | `python -m pytest -m unit` | ~626 | ~3.5min |
| 集成测试 | `python -m pytest -m integration` | ~185 | ~30s |
| 安全测试 | `python -m pytest -m security` | ~54 | ~11s |
| 回归测试 | `python -m pytest -m regression` | ~68 | ~13s |
| 评估测试 | `python -m pytest -m evaluation` | ~58 | ~34s |
| 慢测试 | `python -m pytest -m slow` | 9 | ~30s |
| 外部依赖 | `python -m pytest -m external` | 16 | ~4s |
| 真实 provider | `SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval` | 1 | 需 .env |
| 按文件推荐 | `python scripts/test_impact.py --git` | — | ~1s |
| 覆盖 addopts | `python -m pytest -m "not provider_eval"` | — | — |

**覆盖默认 addopts**：当需要运行被默认排除的 marker 时，使用 `-m` 显式指定，例如 `python -m pytest -m "not provider_eval"` 会覆盖默认的 `addopts` 过滤。

## 测试文件组织

```
tests/
├── conftest.py                    # marker 自动应用 + smoke/flaky/slow 列表 + policy 隔离
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
