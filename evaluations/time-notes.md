# Evaluation 耗时分析（2026-08-14）

> 目标：记录 `sg eval` 时间长的构成与根因，供后续优化对照。
> 数据来源：`evaluations/output/run-1786679743267/results.json`（10 cell = 5 题 × 2 模型，并行上限 10，真实模型调用）。

## 一、时间构成（实测）

| cell | 模型 | 状态 | 总时长(s) | 模型调用(s) | 工具执行(s) | 其他开销(s) |
|---|---|---|---|---|---|---|
| cart-refactor | longcat | timed_out | 1801 | 0 | 0 | 3 |
| invoice-bug | opencode-go | timed_out | 1801 | 74 | 8 | 3 |
| log-stats-feature | longcat | passed | 64 | 1.3 | 59 | 2.8 |
| invoice-bug | longcat | passed | 53 | 1.3 | 49 | 2.6 |
| config-diagnose | longcat | passed | 44 | 0.6 | 40 | 2.7 |
| pipeline-mislead | longcat | passed | 41 | 0.8 | 37 | 2.8 |
| cart-refactor | opencode-go | passed | 37 | 1.3 | 33 | 2.6 |
| log-stats-feature | opencode-go | passed | 30 | 0.7 | 26 | 2.5 |
| config-diagnose | opencode-go | passed | 25 | 1.7 | 20 | 2.6 |
| pipeline-mislead | opencode-go | passed | 22 | 0.7 | 18 | 2.4 |

**正常 cell（8 个）**：21–64 秒。构成：**模型调用 ~21%**（0.6–1.7s，模型思考很快）、**工具执行 ~77%**（bash 进程 + python 测试）、其他开销 ~2.7s（进程启动/协议/判分）。

**超时 cell（2 个）**：卡满 1801s（per-cell 超时上限），**主导 eval 总墙钟 ≈ 30 分钟**（并行下总时长 ≈ 最慢 cell）。

## 二、根因

1. **模型请求挂起 × 重试累积（主要时间黑洞）**：
   - 请求级超时 120s（`PROVIDER_TIMEOUT_SECONDS`，model/src/lib.rs:52）
   - `provider_error_is_retryable` 把 **Timeout 也列为可重试**（model/src/transport.rs:1837-1840），最多 6 次（`MAX_PROVIDER_ATTEMPTS` lib.rs:58）
   - **单次挂起请求最多 120s × 6 = 12 分钟**；agent 多轮叠加 → 卡满 per-cell 超时
   - 例：cart-refactor-longcat 模型调用 0s（请求发出即无响应）；invoice-bug-opencode-go 模型 74s 后挂起
   - provider 侧因素：opencode.ai（Cloudflare 保护）与 longcat API 的间歇性无响应
2. **工具执行占大头（正常 cell 内）**：agent 多轮小步执行，每轮 bash 在 Windows 上 spawn 进程 + python 解释器启动（~0.5–1s/命令）；工具顺序执行（Pi 基线 max_tool_calls=1）
3. **并行上限 10 → 机器压力大**（用户反馈，已改为 5，总时长将变两批但机器稳定）

## 三、优化候选（未实施，按性价比排序）

1. **超时重试策略**：超时（挂起）降为重试次数少（如 6→2）或不再重试（只对 429/5xx 重试）——直接消灭 12 分钟黑洞，预计把最坏 cell 从 1801s 降到 ~240s 以内
2. **per-cell 超时 1800s → 600s**（eval-config.json `timeout_secs`）：配合 1 后足够
3. **增量复用**：`--only-failed`（已通过 cell 不重跑），回归时只跑失败项
4. **工具执行优化**（产品级，单独评估）：bash 长驻复用 / 减少轮数 / 允许小步并行——收益大但改动核心
5. **请求超时 120s → 60s**：降低单次等待上限（配合 1）

## 四、验证方法（优化后重跑对照）

```
sg eval --tasks <题> --models opencode-go/deepseek-v4-flash#max   # 单模型快速对照
对比指标：总墙钟、timed_out 数、正常 cell 平均时长
```

## 五、已落实的改动

- 2026-08-14：并行上限默认 10 → 5（`DEFAULT_MAX_PARALLEL` + eval-config.json `max_parallel: 5`），队列式并发，超出排队

## 超时修复效果（2026-08-14 实测）

- 修复：挂起超时不再重试（transport 层 fail-fast）+ per-cell 超时 1800→600s
- 实测（此前超时的 2 个 cell 重跑，opencode-go）：cart-refactor 1801s→**42s 通过**、invoice-bug 1801s→**44s 通过**
- 成本估算接线验证：cost_estimate 真实计算（$0.0088/$0.0076），非 None

## 2026-08-14 晚间：601s 超时根因调查 + dashscope 切换

### 调查结论（真实链路探针 + 复现）
- **不是 provider 挂起**：mock 服务器探针证实流式请求超时（reqwest 总时长语义）2s 必触发；
  SG_TRACE 逐轮计时证实模型调用全部正常完成（2-19s/轮）
- **真实根因**：模型发**无界命令**（`find / -type d -name shopping` 全盘扫描），bash 工具
  **无默认超时** → 工具永久挂起 → turn 卡死 → eval 600s 杀
- **次生缺陷**：强杀 bash 后残留孙进程（find）因 Windows 句柄继承直写 app-server stdout
  管道，非 UTF-8 字节破坏 JSON-RPC 流（CLI 报 "stream did not contain valid UTF-8"）

### 修复（2026-08-14）
1. bash 工具默认 120s 超时（timeout_ms 可显式覆盖），超时杀进程树返回错误
2. Windows spawn 前清除 stdout/stderr 句柄继承位（防残留进程污染协议流）
3. CLI 读 stdout 容错（lossy + 跳过非 JSON 行）
4. 内置模型表加 dashscope/deepseek-v4-flash-0731（deepseek 官方价）
5. eval 默认模型切 dashscope/deepseek-v4-flash-0731#max
6. results.json 补 total_cost_estimate/total_tokens 汇总

### dashscope eval 实测（5 题，2026-08-14）
- run-1786709731007：**5/5 全部通过**，53s 总时长，251,130 tokens，**cost $0.0377**
- 单 cell：28-53s（此前 opencode-go 慢时 601s 超时、手动 66s）
- 对照（修复前 opencode-go+longcat 10 cell）：6/10 通过、4 个 601s 超时/crash、无稳定成本
