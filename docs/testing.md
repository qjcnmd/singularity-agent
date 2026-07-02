# 测试体系操作手册

## 1. 测试分层

Singularity 测试体系按 pytest marker 分为以下层级。

**Marker 语义**：marker 分为两类——

- **功能分类**（互斥，每个测试必须有且仅有一个）：`unit`、`integration`、`regression`、`security`、`evaluation`
- **运行属性**（可叠加，描述执行特征）：`smoke`、`slow`、`external`、`flaky`、`provider_eval`

| Marker | 测试数 | 分类 | 说明 | 默认是否运行 |
|--------|--------|------|------|-------------|
| `smoke` | 25 | 运行属性 | 核心路径烟雾测试，覆盖 CLI/Context/Planner/Policy/Tool/Verification | ❌ 显式运行 |
| `unit` | ~260 | 功能分类 | 纯函数/类测试，最小化跨组件依赖 | ✅ |
| `integration` | ~589 | 功能分类 | 多组件集成测试（agent simulation、component wiring、subprocess、threading、真实 git） | ✅ |
| `regression` | 68 | 功能分类 | 生产基线、文档一致性、schema 稳定性守卫 | ✅ |
| `security` | 54 | 功能分类 | 信任边界、脱敏、注入、密钥安全测试 | ✅ |
| `flaky` | 4 | 运行属性 | 已知偶发失败测试（默认仍运行，见下方处理策略） | ✅ |
| `evaluation` | 58 | 功能分类 | 评估基础设施：评分、回放、benchmark harness | ❌ 显式运行 |
| `slow` | ~25 | 运行属性 | 真正慢的测试（>3s），agent loop 模拟/并发 | ❌ 显式运行 |
| `external` | ~22 | 运行属性 | 依赖外部资源或平台能力（git/network/Windows OS sandbox APIs），实际通常较快 | ❌ 显式运行 |
| `provider_eval` | 1 | 运行属性 | 需真实模型 provider 的烟雾测试 | ❌ 显式运行 |

> **注意**：数量会随重分类调整而变化。以实际 `python -m pytest --co -q` 为准。

### 自动分类如何工作

`tests/conftest.py` 的 `pytest_collection_modifyitems` 钩子按以下优先级分类：

1. **显式 `@pytest.mark.X`**：开发者手动指定，绝对优先。
2. **`tests/evaluation/` 目录**：硬约定 → `evaluation`。
3. **精选列表**（`_SMOKE_TEST_IDS`、`_FLAKY_TEST_IDS`、`_SLOW_TEST_IDS`、`_EXTERNAL_*`）：手动维护。
4. **关键字推断**：`security/redaction/secret/injection` → `security`；`production/docs_consistency/runtime_docs/runtime_sqlite/singularity_identity` → `regression`；`code_index/diagnostics/edit/interaction/memory/plugins/review/` 子目录 或 文件名含 `integration` → `integration`。
5. **`_INTEGRATION_FILE_STEMS` 精选列表**：手动维护的 35 个集成密集型文件茎。
6. **导入特征推断**（`_module_imports_integration_indicators`）：自动检测模块是否 `import subprocess`、`multiprocessing`、`threading`、或使用了 `make_agent_session`/`AgentLoop`/`DockerSandboxBackend` 等集成符号 → `integration`。
7. **slow/external 后修正**：如果测试已有 `slow`/`external` 标记，绝不落入 `unit`。
8. **其余** → `unit`。

### 新增测试文件注意事项

新增测试时不需要手动标记，但以下情况建议显式指定 `@pytest.mark.integration`：

- 测试中调用真实 AgentLoop（`agent.run()` 或 `make_agent_session()`）
- 测试中使用 `component.run()` / `component.run_plan()` 多组件连线
- 测试中 `import subprocess` / `import multiprocessing` / `import threading`

这些情况通常会被自动推断捕获，但显式标记可确保万无一失。

如果测试耗时 >3s，将其 nodeid 加入 `_SLOW_TEST_IDS` 并运行 `--durations=50` 确认。

## 2. 日常开发

### 烟雾测试（~3 秒，25 tests）

改完代码快速验证核心路径没断：

```bash
python -m pytest -m smoke
```

### 按改动文件推荐测试

```bash
# 自动从 git diff 检测变更文件，推荐相关测试
python scripts/test_impact.py --git

# 指定变更文件
python scripts/test_impact.py src/singularity/context/manager.py src/singularity/planner/engine.py

# JSON 输出（适合 CI 集成）
python scripts/test_impact.py --git --json

# 详细模式（显示映射过程）
python scripts/test_impact.py --git --verbose

# 要求 code index 可用（CI 中确保使用精确分析）
python scripts/test_impact.py --git --strict-index
```

**JSON 输出字段**：

| 字段 | 说明 |
|------|------|
| `changed_files` | 变更文件列表 |
| `source` | `code_index` 或 `path_heuristics` |
| `warnings` | 警告信息列表 |
| `recommended_tests` | 推荐的测试文件列表 |
| `recommended_commands` | 推荐的 pytest 命令 |
| `confidence` | `high`/`medium`/`low` |
| `fallback_gate` | 低置信度或无明确测试时要求升级的 gate，当前为 `stage` |
| `skipped_reason` | 未选择受影响 pytest 时的明确原因 |
| `capability_gate` | 是否因 AgentLoop/ToolProtocol/sandbox/context/compaction/verification/CompletionGate/FinalReport/evaluation runner 变更建议 capability gate |

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

## 3. 本地验证 Gate

### Fast Gate（Codex 小改默认）

日常小改默认运行：

```bash
python scripts/verify_fast.py --git
```

Fast gate 只跑 ruff、当前 mypy、changed-scope compileall，以及 `scripts/test_impact.py` 推荐的受影响 pytest。它不跑全量 pytest，不跑真实 provider eval。若 test impact 为 `low` 或没有明确测试，脚本输出 `fallback_required=stage`、`fallback_reason`、`stage_gate_recommended=true`、selected/skipped test 计数并返回非零码，要求升级 stage gate，不能把空推荐当作通过。

### Stage Gate（阶段收口）

阶段收口运行：

```bash
python scripts/verify_stage.py
```

Stage gate 执行 deterministic 检查：mypy、ruff、`compileall src scripts`、`scripts/verify_runtime_docs.py`、过滤后的 pytest，以及 evaluation runner / test impact / quality gate 专项测试。它输出 ruff/mypy/compileall/pytest/runtime docs 分项耗时，但不默认跑多个真实 provider eval。

### Capability Gate（核心链路变更）

只有影响 AgentLoop、ToolProtocol、sandbox、context、compaction、verification、CompletionGate、FinalReport 或 evaluation runner 时运行：

```bash
python scripts/verify_capability.py --force --run-id <run-id>
```

该 gate 只运行 `docs/evaluation/public-representative-task.json` 中的 `sqlfluff__sqlfluff-2419` 这一项公共 SWE-bench Lite dev 任务；旧真实任务和旧多任务 regression manifest 不属于默认路径。result 中的 `capability_summary.schema_version` 为 `evaluation.capability_summary/v2`，除 model/tool/retrieval/context/compaction/sandbox/verification/finalization 对象流外，还输出 `provider_time_by_turn`、逐命令 `sandbox_commands`、互斥 `wall_phases`、`unattributed_time_seconds`、细分 timing 和 `timing_diagnostics`。可靠数据不存在时 timing 必须为 `null`，并以 `unavailable` 或 `not_applicable` 说明原因，不能用假 `0` 补齐。

公共任务必须先证明 base commit + evaluator test patch 失败，agent patch 后同一验证通过，且 agent completed、目标文件变化、public/hidden verification、FAIL_TO_PASS、CompletionGate、sandbox enforcement、evaluator visibility audit 全部通过，才允许 `evaluation_passed=true`。sandbox-required 任务要求 `local_process_fallback_count=0`；evaluator-only metadata 不得进入模型可见 task projection 或 AgentLoop trace。没有触发 compaction 时必须写明 skipped reason，例如 context usage 未到阈值、retrieval 内容不足或任务过早完成。result 同时写出 `evaluation_metrics` scorecard，`verify_capability.py` 会把 resolved、tool success rate、cost 和 pricing 状态摘要放入 JSON 输出；这些字段只用于诊断/回归分析，不改变 capability gate 的 pass/fail 判断，也不会因为 cost/pricing unknown 导致失败。

`capability_sla` 是 Phase 8.5 起的非阻断诊断层，写入 task result、summary、report 和 `verify_capability.py` JSON 输出，但不改变 `evaluation_passed`、`tests_passed` 或脚本退出码。当前目标为 wall <= 300s、AgentLoop <= 210s、provider <= 55s、sandbox <= 50s、dependency setup <= 35s；verification、unattributed time、local fallback 和 visibility audit 也会显示在 SLA 表中。SLA 超限必须报告 `over_sla` 和 delta，但不得通过减少 public/hidden verification、放宽 CompletionGate、扩大 ACL、绕过 Windows sandbox 或引入 local process fallback 来压时间。dependency setup 和 sandbox readiness 的优化只允许使用安全、可审计、可失效的缓存或现有 provider/native cache 诊断；当前 runner 只对 evaluator-only prepare commands 注入 `PIP_CACHE_DIR`，并在 `reproducible_environment.dependency_setup_cache` 记录 key、cache dir、失效输入和安全边界。

仍可按需运行以下传统 pytest 命令，但它们不再是 Codex 小改默认 fast gate：

```bash
# 默认 pytest（排除 evaluation、provider_eval、slow、external）
python -m pytest -m "not evaluation and not provider_eval and not slow and not external"

# 真正快速 unit（仅纯函数/类测试，排除 slow/external 中的集成测试）
python -m pytest -m "unit and not slow and not external"

# 带覆盖率
python -m pytest --cov=singularity --cov-report=term-missing
```

## 4. Release Full Gate

```bash
# 1. 全量测试（含 slow + external，排除 provider_eval）
python -m pytest -m "not provider_eval"

# 2. 安全测试单独确认
python -m pytest -m security -v

# 3. 回归测试单独确认
python -m pytest -m regression -v

# 4. 评估基础设施测试
python -m pytest -m evaluation -v

# 5. 真实 provider 测试（需要配置 .env）
SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval -v

# 6. 真实 public capability benchmark
python scripts/verify_capability.py --force --run-id release-smoke
```

## 5. 本地 provider_eval

`provider_eval` 测试需要真实模型 provider 配置。在项目根目录创建 `.env` 文件：

```
SINGULARITY_API_KEY=your-api-key
SINGULARITY_BASE_URL=https://your-provider-endpoint
SINGULARITY_MODEL=your-model-name
```

运行：

```bash
SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval -v
```

> **安全提醒**：不要提交 `.env` 文件。API key 和 secret 不要出现在日志、截图或 markdown 中。

## 6. Flaky 测试处理策略

### 当前 Flaky 清单

| 测试 | 失败现象 | 疑似原因 | 退出条件 |
|------|----------|----------|----------|
| `test_cli.py::test_cli_eval_task_validate_and_list_filter_tags` | 偶发超时 | CLI eval 依赖 benchmark adapter 响应时间 | 连续 20 次无失败 |
| `test_cli.py::test_cli_eval_private_uses_private_benchmark_adapter` | 偶发超时 | 同上 | 连续 20 次无失败 |
| `test_tool_executor_secret_safety.py::test_list_files_hides_sensitive_paths_by_default` | AssertionError | 测试间共享全局状态导致顺序依赖 | 修复 fixture 隔离后验证 20 次无失败 |
| `test_observability_integration.py::test_tool_executor_dispatch_emits_structured_trace` | AssertionError | 顺序依赖，trace event 时序不确定 | 修复 fixture 隔离后验证 20 次无失败 |

### 策略

- **默认仍包含**：flaky 测试不从默认运行中排除，因为它们保护重要路径（CLI eval、secret safety、observability）。
- **失败时重跑一次**：提交前如果 flaky 测试失败，重跑 `python -m pytest <test_path>` 确认。
- **连续两次失败 = 真实回归**：需要修复。
- **退出条件**：每个 flaky 测试都有明确的退出条件（见上表）。满足退出条件后，从 `_FLAKY_TEST_IDS` 中移除。

## 7. Slow/External 运行方式

### Slow 测试

```bash
# 运行所有 slow 测试
python -m pytest -m slow -v

# 运行 slow + 默认测试
python -m pytest -m "not evaluation and not provider_eval and not external"
```

Slow 测试主要是 agent loop 多轮模拟（3-8s）和并发测试。日常开发不需要运行，提交前或 CI 中运行。

### External 测试

```bash
# 运行所有 external 测试
python -m pytest -m external -v
```

External 测试依赖 git、network 或Windows OS sandbox能力。本地与CI都必须按实际平台能力运行；backend capability/setup缺失时应断言`backend_unavailable`，不能改用普通本地进程获得通过。

`tests/evaluation/test_targeted_failure_replay.py` 需要可用的 OS sandbox 才能完成真实 verification/repair 回放，因此同时标记为 `evaluation` 和 `external`。sandbox 未完成 setup 时测试必须明确 `skip` 并保留 `sandbox backend unavailable` 原因，不能切换到普通本地进程或伪造完成。

Windows sandbox定向验证：

```bash
python -m pytest -m external tests/test_sandbox_backend_windows.py -v
python -m pytest tests/test_sandbox_backend_windows.py -m "" -v
python -m pytest tests -k sandbox -m "not evaluation and not provider_eval and not slow" -v
```

当前 Windows 测试覆盖 v2 doctor/setup/cleanup schema、offline/online账户路由、登录UI隐藏、logon deny rights、受限组成员、独立credential、offline firewall与online排除、runner身份hash、denied/allowed network probe、legacy迁移、超长legacy账户删除、state dir ACL/完整性/只读属性恢复、幂等cleanup、cleanup residual audit、未完成elevated setup时的fail-closed、backend unavailable不启动进程、`CommandExecutor -> SandboxManager -> WindowsSandboxBackend`的fake-ready成功链路、protected path preflight、timeout/Job Object evidence、输出脱敏与runner evidence不能被硬编码伪造。

真实 Windows account-backed sandbox 需要先在 elevated shell 运行：

```bash
python -m singularity.cli sandbox setup --json
python -m singularity.cli sandbox doctor --json
```

setup 必须创建/验证`SingularityOffline`与`SingularityOnline`两个本地账户及独立Windows Credential Manager凭据；两者都必须隐藏于标准登录用户列表，保留runner所需interactive logon right，同时拒绝RDP/network/service/batch logon并限制直接本地组为内置Users。`Singularity Sandbox`firewall group的outbound block只绑定offline SID；doctor还必须证明offline网络被拒绝、online网络可用、两个runner身份与SID hash匹配。`SingularitySandbox`和`SingularitySandboxRunner`只作为迁移清理目标，不进入运行路径；任一当前能力缺失或legacy资产残留都保持`backend_unavailable`。

当前 Windows backend 已支持 machine state下的短run root、workspace COW projection、protected path exclude、仅选中sandbox账户加宿主cleanup ACE的run root ACL、workspace-only low-integrity label、private desktop、restricted token、Job Object、timeout、output limit、secret redaction、临时输出/属性恢复清理、artifact refs和带host baseline的network denied proof。普通权限doctor遇到LSA access denied时只接受ACL受保护且匹配当前双SID hash的setup证明。workspace外additional writable directories与path-specific readonly leases仍fail closed。

## 8. Marker 自检方式

### 运行测试策略自检

```bash
# 运行自检测试
python -m pytest tests/test_test_infra.py -v
```

自检测试验证：
- `_SMOKE_TEST_IDS` 中每个 nodeid 在测试集合中存在
- `_FLAKY_TEST_IDS`、`_SLOW_TEST_IDS` 同理
- `_SMOKE_FILE_STEMS`、`_EXTERNAL_FILE_STEMS`、`_INTEGRATION_FILE_STEMS` 同理
- smoke 与 slow/flaky/external/provider_eval 无交集
- slow/external 测试必须有非 unit 功能分类（integration/regression/security/evaluation）
- 所有 marker 值都在 `_KNOWN_MARKERS` 中
- `_KNOWN_MARKERS` 与 `pyproject.toml` 定义一致
- 各 marker 数量在合理范围内（±30% 容差）

### 运行时自检

`conftest.py` 中的 `_validate_curated_lists` 函数在每次测试收集时自动运行。如果精选列表中有失效的 nodeid，会通过 `pytest.PytestWarning` 发出警告。

```bash
# 将警告提升为错误（CI 中推荐）
python -m pytest -W error::pytest.PytestWarning -m smoke
```

## 9. Test Impact 分析

### 基本用法

```bash
# 从 git diff 自动检测变更
python scripts/test_impact.py --git

# 指定文件
python scripts/test_impact.py src/singularity/context/manager.py

# 对比 main 分支
python scripts/test_impact.py --git --base main
```

### 输出格式

```bash
# JSON 输出（适合 CI 集成）
python scripts/test_impact.py --git --json

# 详细模式（显示映射过程）
python scripts/test_impact.py --git --verbose

# 要求 code index（CI 中确保精确分析）
python scripts/test_impact.py --git --strict-index
```

### Confidence 级别

| 级别 | 条件 | 含义 |
|------|------|------|
| `high` | code index 可用且有命中 | 推荐结果基于精确的代码依赖分析 |
| `medium` | code index 不可用但 fallback 有命中 | 推荐结果基于路径命名约定 |
| `low` | 无命中 | 无法确定相关测试，fast gate 必须回退 stage gate |

### 特殊路径映射

以下文件不是测试文件，但变更时会触发对应测试：

| 变更文件 | 推荐测试 | 说明 |
|----------|----------|------|
| `src/singularity/agent_loop.py` | `tests/test_agent.py`, `tests/test_agent_task_outcome.py` | AgentLoop 主链路 |
| `src/singularity/tool_protocol/engine.py` | `tests/test_tool_protocol_engine.py` | ToolProtocol engine |
| `src/singularity/evaluation/runner.py` | `tests/evaluation/test_evaluation_runner.py` | evaluation runner |
| `src/singularity/kernel/finalization.py` | `tests/test_kernel_finalization.py` | FinalReport / kernel finalization |
| `src/singularity/tools/verification.py` | `tests/test_verification_runner.py` | verification tool |
| `scripts/test_impact.py` | `tests/test_test_impact.py` | 脚本自身测试 |
| `scripts/verify_fast.py` / `scripts/verify_stage.py` / `scripts/verify_capability.py` | `tests/test_quality_gates.py` | 本地 gate 合同 |
| `tests/conftest.py` | `tests/test_test_infra.py` | marker/精选列表配置测试 |
| `docs/testing.md` | `tests/test_docs_consistency.py` | 文档一致性测试 |
| `pyproject.toml` | `tests/test_test_infra.py` | pytest 配置测试（附加 `--collect-only -m smoke` 提示） |
| `scripts/verify_runtime_docs.py` | `tests/test_runtime_docs_verify.py` | 文档验证脚本测试 |

**推荐结果验证**：`recommended_tests` 中的所有条目都必须是 pytest 可收集的测试文件（`tests/test_*.py`），不含 `conftest.py`、`__init__.py`、`*_helpers.py` 等非测试文件。

### 运行自身测试

```bash
python -m pytest tests/test_test_impact.py -v
```

## 10. 耗时基线

| 验证层级 | 入口 | 目标耗时 | 告警阈值 | 维护规则 |
|----------|------|----------|----------|----------|
| fast gate | `python scripts/verify_fast.py --git` | 按变更，通常 <2min | 无法选择测试时回退 stage | ruff/mypy/changed compileall/受影响 pytest，不跑真实 provider |
| stage gate | `python scripts/verify_stage.py` | <6min | >8min | deterministic 收口，含 runtime docs 和过滤 pytest |
| capability gate | `python scripts/verify_capability.py --force --run-id <id>` | 依 provider 与 sandbox | 阻塞必须分类清楚 | 只跑公共代表性真实任务 |
| smoke | `python -m pytest -m smoke` | <10s | >15s | 检查是否有测试变慢 |
| release-full | `python -m pytest -m "not provider_eval"` | <6min | >8min | 排查慢测试 |
| fast-unit | `python -m pytest -m "unit and not slow and not external"` | <2min | >3min | 重分类或优化 |
| evaluation infra | `python -m pytest -m evaluation` | <1min | >2min | 只验证评估基础设施，不代表真实 provider |

### 检查方法

```bash
# 查看最慢 50 个测试
python -m pytest --durations=50

# 查看最慢 50 个 unit 测试
python -m pytest -m unit --durations=50

# 查看最慢 50 个 smoke 测试
python -m pytest -m smoke --durations=50
```

### 维护规则

1. **每次 Release** 运行 `--durations=50`，更新基线表。
2. **新增测试** 如果 >3s，考虑标记为 `slow`。
3. **重分类** 基于实际测量数据，不基于文件名启发式。
4. **告警** 超过告警阈值时，排查是否是测试变慢还是环境问题。

## 11. Smoke 覆盖矩阵

每个 smoke 测试覆盖的核心运行时路径：

| Smoke 测试 | 覆盖路径 | 说明 |
|------------|----------|------|
| `test_cli_runs_through_kernel_bootstrap` | CLI → KernelBootstrap | CLI 启动入口 |
| `test_context_manager_initializes_system_and_user_messages` | Context 组装 | 上下文初始化 |
| `test_start_task_builds_state_plan_and_persists` | Planner 决策 | 任务规划 |
| `test_interactive_approve_once_generates_single_use_grant` | Policy/Approval 门控 | 审批流程 |
| `test_verification_runner_executes_checks_through_command_executor_and_records_trace` | Verification 链路 | 验证执行 |
| `test_policy_engine` (12 tests) | Policy 决策全分支 | 策略引擎 |
| `test_tool_contract` (5 tests) | Tool 合约/协议 | 工具契约 |
| `test_trace` (1 test) | Trace 基础写入 | 可观测性 |
| `test_tool_protocol_result` (2 tests) | Tool 结果构建 + 脱敏 | 工具协议 |

### 覆盖的运行时子系统

- **CLI bootstrap** ✅
- **AgentLoop outcome** ✅（通过 verification/planner tests 间接覆盖）
- **Planner** ✅
- **Tool contract/protocol** ✅
- **Policy/Approval** ✅
- **Verification** ✅
- **Context** ✅
- **Trace** ✅
- **Test Impact** — 建议补充轻量 smoke（见下方）

### 补充 Test Impact Smoke

在 `tests/test_test_impact.py` 中增加：

```python
def test_test_impact_fallback_basic_mapping() -> None:
    """Smoke: verify basic path heuristic mapping works."""
    result, _ = _fallback_tests(["src/singularity/cli.py"])
    assert "tests/test_cli.py" in result
```

加入 `_SMOKE_TEST_IDS`：

```python
"tests/test_test_impact.py::test_test_impact_fallback_basic_mapping",
```

## 12. 关键保护边界

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

## 测试命令速查

| 场景 | 命令 | 测试数 | 耗时 |
|------|------|--------|------|
| 日常核心路径 smoke | `python -m pytest -m smoke` | 25 | ~3s |
| 提交前默认 gate | `python -m pytest` | ~900 | ~2.5min |
| 真正快速 unit | `python -m pytest -m "unit and not slow and not external"` | ~260 | ~20s |
| release-full | `python -m pytest -m "not provider_eval"` | ~1030 | ~3.5min |
| 集成测试 | `python -m pytest -m integration` | ~589 | ~2min |
| 安全测试 | `python -m pytest -m security` | 54 | ~11s |
| 回归测试 | `python -m pytest -m regression` | 68 | ~13s |
| 评估测试 | `python -m pytest -m evaluation` | 58 | ~34s |
| 慢测试 | `python -m pytest -m slow` | ~25 | ~1min |
| 外部依赖 | `python -m pytest -m external` | ~22 | ~4s |
| 真实 provider | `SINGULARITY_RUN_PROVIDER_EVAL=1 python -m pytest -m provider_eval` | 1 | 需 .env |
| 按改动推荐 | `python scripts/test_impact.py --git --json` | 按变动 | ~1s |
| 自检 | `python -m pytest tests/test_test_infra.py -v` | ~10 | ~5s |

## 测试文件组织

```
tests/
├── conftest.py                    # marker 自动应用 + smoke/flaky/slow 列表 + policy 隔离 + 自检
├── agent_loop_helpers.py          # AgentLoop 测试辅助
├── tool_executor_helpers.py       # ToolExecutor 测试辅助
├── test_test_infra.py             # 测试策略自检（marker 验证、curated list 验证）
├── test_test_impact.py            # test impact 脚本自身测试
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
