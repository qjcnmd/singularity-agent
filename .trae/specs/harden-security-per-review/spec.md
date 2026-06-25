# 根据 SINGULARITY_CODE_REVIEW.md 审查报告的安全与健壮性加固 Spec

## Why

`C:\Users\Lenovo\Desktop\SINGULARITY_CODE_REVIEW.md` 全量审查发现 4 个 P0 阻塞上线问题、5 个 P1 高优先级问题和 22 个 P2 中优先级问题。其中 P0-1（审批 grants 存放于模型可写工作区，可被模型自伪造绕过人工审批）是系统性安全缺陷；P0-4（npm 失败解析器 group 索引错误）会中断失败分析管线。本 spec 覆盖 P0 全部、P1 全部以及与并发/资源/数据污染相关的关键 P2 项，确保框架达到生产可用安全姿态。

所有 P0/P1 关键发现均已人工复核源码确认，行号与报告一致。

## What Changes

### P0 — 阻塞上线（必须修复）

- **P0-4**：修复 `verification/parsers.py` npm parser group 索引错误（`group(2)` → `group(3)`）
- **P0-1**：将审批 grants/audit 默认存储路径移出模型可写工作区（改为 `~/.singularity/policy/`），并在 PolicyEngine 命令通道对工作区内 `.singularity/policy/` 路径加 hard-deny
- **P0-2**：远程 grant 导入引入 request_digest 校验与 scope 收敛校验（`grant.scope ⊆ decision.required_approval.scope`）
- **P0-3**：grant 去重改为 `decision_id` + `nonce` 维度，`from_dict` 在缺 `grant_id` 时不再静默生成新 ID，而是要求来源可信的稳定标识

### P1 — 高优先级（安全/可用性）

- **P1-1**：扩充 `SECRET_PATTERNS`（AWS Access Key `AKIA`、JWT `eyJ`、Slack `xoxb-`、Stripe `sk_live_`、Google `AIza`）；`add_tool_result` 对所有 sensitivity 统一过 redactor；补全 dict 分支敏感字段名（`authorization`/`credential`/`passphrase`/`private_key`/`access_token`）
- **P1-2**：`network.mode=DENIED` 在无网络隔离后端上 fail-closed（产生 SandboxViolation）；修正 read-only 映射 bug（不再映射为 `COPY_ON_WRITE_WORKSPACE`）；LocalStagingBackend `capabilities()` 降级 `filesystem_isolation=False`
- **P1-3**：Docker 沙箱加固：`--user`、`--cap-drop=ALL`、`--security-opt no-new-privileges`、`--init`、镜像 digest pinning、`--memory`/`--pids-limit`；超时后 `docker stop` 清理孤儿容器
- **P1-4**：`read_file`/`search_text` 加大小上限与流式截断；只读工具缓存键改用 `invalidate_paths` 增量失效而非全工作区 sha256 哈希
- **P1-5**：给 `ToolExecutor._cache`/`_ledger` 加 `threading.RLock`

### P2 — 中优先级（并发/资源/数据污染，选关键项）

- **P2-10**：`ContextStore` SQLite 加 `check_same_thread=False`，读路径持锁
- **P2-11**：`PlannerStore._write_json` 改原子写入（tempfile+os.replace+fsync），`append_event` 加文件锁
- **P2-17**：`WorkspaceMutationManager` 五个容器加上限（LRU 淘汰），journal `.before` 制品加清理
- **P2-18**：live eval patch diff 跳过 `.env` 及敏感文件，或强制 redact
- **P2-19**：`register_file_artifact(sensitive=False)` 对源文件过 redactor
- **P2-20**：`TraceStore` 校验 `run_id` 路径安全性（禁止 `..`/绝对路径）
- **P2-21**：`SpanManager._stack` 改线程局部存储

## Impact

- Affected code:
  - `src/singularity/verification/parsers.py`（P0-4）
  - `src/singularity/policy/approval.py`、`src/singularity/policy/models.py`、`src/singularity/policy/remote.py`、`src/singularity/policy/config.py`、`src/singularity/policy/rules.py`（P0-1/P0-2/P0-3）
  - `src/singularity/tools/executor.py`（P0-1 grant 消费前校验来源、P1-4 缓存键、P1-5 加锁）
  - `src/singularity/context/redaction.py`、`src/singularity/context/manager.py`、`src/singularity/observability/redaction.py`（P1-1）
  - `src/singularity/sandbox/manager.py`、`src/singularity/sandbox/backends.py`、`src/singularity/sandbox/filesystem.py`（P1-2/P1-3）
  - `src/singularity/tools/read_only.py`（P1-4）
  - `src/singularity/context/store.py`、`src/singularity/planner/store.py`、`src/singularity/workspace/mutation_manager.py`、`src/singularity/evaluation/live.py`、`src/singularity/observability/artifacts.py`、`src/singularity/observability/store.py`、`src/singularity/observability/spans.py`（P2 项）
- Affected Runtime Flow Docs:
  - `docs/architecture/modules/policy-approval-gates.md`（P0-1/P0-2/P0-3 改变审批 grant 存储与校验行为）
  - `docs/architecture/modules/tool-execution-runtime.md`（P0-1 grant 来源校验、P1-4/P1-5 工具缓存与并发）
  - `docs/architecture/modules/tool-registry-exposure.md`（P1-4 只读工具资源边界）
  - `docs/architecture/modules/context-compaction-observation-store.md`（P1-1 脱敏、P2-10 ContextStore 并发）
  - `docs/architecture/modules/trace-observation-audit-events.md`（P2-19/P2-20/P2-21 观测层加固）
- 不改动的运行时行为：AgentLoop 编排、Planner 决策、ModelRunner 请求构造、ContextAssembler 检索逻辑。

## ADDED Requirements

### Requirement: 审批 grants 与审计日志须存放于模型不可写位置

系统 SHALL 将审批 grants 与审计日志默认存放于工作区之外（`~/.singularity/policy/`），且 PolicyEngine 命令通道 SHALL 对工作区内 `.singularity/policy/` 路径的写操作 hard-deny。ToolExecutor 在消费 grant 前 SHALL 校验 grant 来源可信（路径在工作区外或经签名验证）。

#### Scenario: 模型尝试通过 shell 写入工作区内 approval_grants.jsonl
- **WHEN** 模型通过命令工具执行 `echo >> .singularity/policy/approval_grants.jsonl`
- **THEN** PolicyEngine 在命令通道对该路径 hard-deny，返回 `PolicyDenied`
- **AND** 该写操作不生效

#### Scenario: 默认 grants 路径在工作区外
- **WHEN** 系统初始化且未显式配置 `approval_grants_path`
- **THEN** grants 文件路径解析为 `~/.singularity/policy/approval_grants.jsonl`
- **AND** 不在工作区 `workspace_root/.singularity/policy/` 下

### Requirement: 远程审批 grant 须密码学校验与 scope 收敛

`import_grant` SHALL 校验 `request_digest` 与重新计算的摘要一致，且 `grant.scope` 是 `decision.required_approval.scope` 的子集。未通过校验的 grant SHALL 被拒绝。

#### Scenario: 导入伪造 scope 全开的 grant
- **WHEN** 导入的 grant 的 scope 包含 `path_globs=["*"]` 但原 decision 的 required scope 仅限单文件
- **THEN** `import_grant` 抛出 `ValueError`
- **AND** grant 不被注册

### Requirement: npm 失败解析器须正确提取行号

`NpmBuildFailureParser.parse` SHALL 使用正确的正则 group 索引提取行号（`group(3)`），不得尝试将文件扩展名转为整数。

#### Scenario: 解析 npm 构建错误行
- **WHEN** 输出包含 `src/app.tsx:12:5 - error TS2304`
- **THEN** `ParsedFailure.line == 12`
- **AND** 不抛出 `ValueError`

### Requirement: 只读工具须有资源边界

`read_file` SHALL 在读取前检查文件大小并在超限时截断；`search_text` SHALL 限制单次扫描文件大小。只读工具缓存 SHALL 使用基于 `invalidate_paths` 的增量失效，不得对整个工作区做 sha256 哈希。

### Requirement: 沙箱 read-only 模式须真正只读

`read_only`/`readonly` 文件系统模式 SHALL 映射为真正的只读强制，不得映射为 `COPY_ON_WRITE_WORKSPACE`。无网络隔离能力的后端在 `network.mode=DENIED` 时 SHALL fail-closed。

### Requirement: 并发数据结构须线程安全

`ToolExecutor._cache`/`_ledger`、`SpanManager._stack`、`ContextStore` SQLite 访问 SHALL 在并发访问下线程安全。

## MODIFIED Requirements

### Requirement: 审批 grant 去重

`register_grant` SHALL 按 `decision_id` + `nonce`（若有）去重，而非仅按 `grant_id`。`ApprovalGrant.from_dict` 在缺 `grant_id` 时 SHALL 基于决策内容生成确定性 ID，不得每次生成随机新 ID。

### Requirement: 密钥脱敏覆盖

`SECRET_PATTERNS` SHALL 覆盖 AWS Access Key、JWT、Slack、Stripe、Google API Key 等主流密钥格式。`add_tool_result` SHALL 对所有 sensitivity 级别统一过 redactor。

## REMOVED Requirements

### Requirement: grants 默认存放于工作区内

**Reason**: 模型可写工作区内的 grants 可被自伪造，违反"模型不可自授权"核心安全假设（P0-1）。
**Migration**: 默认路径迁移至 `~/.singularity/policy/`；`PolicyConfig.approval_grants_path` 仍可显式覆盖以兼容已有部署。
