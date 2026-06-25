# Tasks

## Phase 1 — P0 阻塞上线修复

- [x] Task 1: 修复 npm 失败解析器 group 索引错误 (P0-4)
  - [x] SubTask 1.1: 修改 `src/singularity/verification/parsers.py` 的 `NpmBuildFailureParser.parse`，将 `int(match.group(2))` 改为 `int(match.group(3))`
  - [x] SubTask 1.2: 在 `tests/test_verification_runner.py` 或同级测试文件中添加 npm 构建错误行解析测试，覆盖 `src/app.tsx:12:5 - error TS2304` 场景，断言 `ParsedFailure.line == 12` 且不抛异常

- [x] Task 2: 审批 grants/audit 默认路径移出工作区 (P0-1)
  - [x] SubTask 2.1: 修改 `src/singularity/policy/approval.py` 的 `_approval_grants_path`，默认路径改为 `Path.home() / ".singularity" / "policy" / "approval_grants.jsonl"`，仅在显式配置 `approval_grants_path` 时使用配置值
  - [x] SubTask 2.2: 修改 `src/singularity/policy/config.py` 中 audit 日志默认路径同样移出工作区
  - [x] SubTask 2.3: 修改 `src/singularity/policy/rules.py`，在命令通道对工作区内 `.singularity/policy/` 子路径的写操作加 hard-deny 规则（在现有 denied_dirs 之前求值）
  - [x] SubTask 2.4: 修改 `src/singularity/tools/executor.py` grant 消费逻辑，在 `consume_matching_grant` 命中后校验 grant 来源路径在工作区外或经签名验证；不可信 grant 不放行
  - [x] SubTask 2.5: 更新 `tests/test_approval_gate.py` / `tests/test_policy_engine.py`，验证默认路径在工作区外、模型 shell 写 `.singularity/policy/` 被 hard-deny、不可信 grant 不被消费

- [x] Task 3: 远程 grant 引入 digest 校验与 scope 收敛 (P0-2)
  - [x] SubTask 3.1: 修改 `src/singularity/policy/remote.py` 的 `import_grant`，读取 payload 中的 `request`/`decision`/`request_digest`，重新计算摘要并比对；不一致抛 `ValueError`
  - [x] SubTask 3.2: 在 `import_grant` 中校验 `grant.scope` 是 `decision.required_approval.scope` 的子集（capabilities/path_globs/command_patterns/network_hosts 逐项收敛），不收敛抛 `ValueError`
  - [x] SubTask 3.3: 修改 `export_request` 同时导出 `decision.required_approval.scope` 供导入侧校验
  - [x] SubTask 3.4: 在 `tests/test_remote_approval.py` 添加伪造 scope 全开 grant、digest 篡改的拒绝用例

- [x] Task 4: grant 去重改为 decision_id + nonce 维度 (P0-3)
  - [x] SubTask 4.1: 修改 `src/singularity/policy/models.py` 的 `ApprovalGrant.from_dict`，缺 `grant_id` 时基于 `decision_id` + `request_id` + `approved_by` 生成确定性 ID（hash），而非随机 uuid
  - [x] SubTask 4.2: 修改 `src/singularity/policy/approval.py` 的 `register_grant`/`consume_matching_grant` 去重逻辑，改为按 `decision_id` 去重（同一决策只能有一个活跃 grant）
  - [x] SubTask 4.3: 在 `tests/test_approval_gate.py` 添加反复 import 同一 grant 不得放大为多个可消费 grant 的测试

## Phase 2 — P1 高优先级修复

- [x] Task 5: 扩充密钥脱敏覆盖 (P1-1)
  - [x] SubTask 5.1: 修改 `src/singularity/context/redaction.py` 的 `SECRET_PATTERNS`，新增 AWS Access Key (`AKIA[0-9A-Z]{16}`)、JWT (`eyJ[A-Za-z0-9_-]+\.eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+`)、Slack (`xox[baprs]-[A-Za-z0-9-]+`)、Stripe (`sk_live_[A-Za-z0-9]+`)、Google (`AIza[0-9A-Za-z_-]{35}`)
  - [x] SubTask 5.2: 修改 `redact_value` 的 dict 分支，补全敏感字段名集合（`authorization`/`credential`/`passphrase`/`private_key`/`access_token`/`refresh_token`/`client_secret`）
  - [x] SubTask 5.3: 修改 `src/singularity/context/manager.py` 的 `add_tool_result`，对所有 sensitivity 级别统一过 redactor（不再仅对 SECRET/SENSITIVE 脱敏）
  - [x] SubTask 5.4: 同步更新 `src/singularity/observability/redaction.py` 的 token 正则
  - [x] SubTask 5.5: 在 `tests/test_context_redaction.py` 添加 AWS/JWT/Slack/Stripe/Google 密钥脱敏用例

- [x] Task 6: 沙箱 read-only 映射修复与 fail-closed (P1-2)
  - [x] SubTask 6.1: 修改 `src/singularity/sandbox/manager.py:274-275`，`read_only`/`readonly` 不再映射为 `COPY_ON_WRITE_WORKSPACE`，改为映射为 `READ_ONLY_WORKSPACE` 并在执行时强制只读挂载/拒绝写
  - [x] SubTask 6.2: 修改 `src/singularity/sandbox/backends.py` 的 `LocalStagingBackend.capabilities()`，将 `filesystem_isolation` 降级为 `False`
  - [x] SubTask 6.3: 修改 `src/singularity/sandbox/manager.py` 的 `ensure_capabilities`，当 `network.mode=DENIED` 且选中后端无 `network_isolation` 时产生 `SandboxViolation`（fail-closed）
  - [x] SubTask 6.4: 修改 `capability_summary` 基于选中后端而非全部后端并集
  - [x] SubTask 6.5: 在 `tests/test_sandbox_manager.py` / `test_sandbox_backend_local.py` 添加 read-only 真正只读、network DENIED fail-closed、capabilities 降级测试

- [x] Task 7: Docker 沙箱加固 (P1-3)
  - [x] SubTask 7.1: 修改 `src/singularity/sandbox/backends.py` 的 Docker 后端，添加 `--user`（非 root）、`--cap-drop=ALL`、`--security-opt no-new-privileges`、`--init` 参数
  - [x] SubTask 7.2: 添加镜像 digest pinning 支持（优先使用 digest 而非 tag）
  - [x] SubTask 7.3: 添加 `--memory`/`--pids-limit` 资源上限（从 profile.resources 读取）
  - [x] SubTask 7.4: 超时后在异常处理中调用 `docker stop` 清理孤儿容器
  - [x] SubTask 7.5: 在 `tests/test_sandbox_backend_docker.py` 添加加固参数构造校验测试（可 mock docker CLI）

- [x] Task 8: 只读工具资源边界与缓存键优化 (P1-4)
  - [x] SubTask 8.1: 修改 `src/singularity/tools/read_only.py` 的 `read_file`，读取前 `os.path.getsize` 检查，超限（如 1MB）时只读取前 max_bytes 并返回截断标记
  - [x] SubTask 8.2: 修改 `search_text`，单文件扫描前检查大小，超限跳过并记录警告
  - [x] SubTask 8.3: 修改 `src/singularity/tools/executor.py:1252-1306` 的只读工具缓存键，改为基于 `invalidate_paths`（变更影响的路径集合）的增量失效，移除对整个工作区 `rglob("*")` + `read_bytes()` + sha256 的逻辑
  - [x] SubTask 8.4: 写工具执行后的 `self._cache.clear()` 改为只失效受影响路径的条目
  - [x] SubTask 8.5: 在 `tests/test_tools.py` / `test_tool_executor_cache.py` 添加大文件截断、缓存增量失效测试

- [x] Task 9: ToolExecutor 缓存/账本加锁 (P1-5)
  - [x] SubTask 9.1: 修改 `src/singularity/tools/executor.py`，给 `self._cache`/`self._ledger` 添加 `threading.RLock`，所有访问点（get/set/move_to_end/popitem/clear）持锁
  - [x] SubTask 9.2: 在 `tests/test_tool_executor_cache.py` 添加多线程并发访问 cache/ledger 不抛异常的测试

## Phase 3 — P2 关键并发/资源/数据污染修复

- [x] Task 10: ContextStore SQLite 并发安全 (P2-10)
  - [x] SubTask 10.1: 修改 `src/singularity/context/store.py`，SQLite 连接加 `check_same_thread=False`，读路径持锁
  - [x] SubTask 10.2: 在 `tests/test_context_store_production.py` 添加并发读写测试

- [x] Task 11: PlannerStore 原子写入与文件锁 (P2-11)
  - [x] SubTask 11.1: 修改 `src/singularity/planner/store.py` 的 `_write_json`，改为 tempfile + os.replace + fsync 原子写入
  - [x] SubTask 11.2: `append_event` 加文件锁（参考 memory 层实现）
  - [x] SubTask 11.3: 在 `tests/test_planner.py` 添加原子性与并发追加测试

- [x] Task 12: WorkspaceMutationManager 容器加上限与 journal 清理 (P2-17)
  - [x] SubTask 12.1: 修改 `src/singularity/workspace/mutation_manager.py`，五个容器（pending/committed/rolled_back 等）加 LRU 上限（如 1000 条），超限淘汰最旧
  - [x] SubTask 12.2: journal `.before` 制品在变更成功提交后加清理（保留最近 N 条用于 rollback）
  - [x] SubTask 12.3: 在 `tests/test_workspace_mutation.py` 添加无界增长防护测试

- [x] Task 13: live eval patch diff 脱敏 (P2-18)
  - [x] SubTask 13.1: 修改 `src/singularity/evaluation/live.py:1837-1851,1895-1897`，patch diff 生成时跳过 `.env`/`*.pem`/`*.key` 或强制过 redactor
  - [x] SubTask 13.2: 在 `tests/evaluation/test_scoring_replay_harness.py` 添加 `.env` 内容不泄入 `result.json` 的测试

- [x] Task 14: register_file_artifact 脱敏 (P2-19)
  - [x] SubTask 14.1: 修改 `src/singularity/observability/artifacts.py:120-121`，`register_file_artifact(sensitive=False)` 时对源文件内容过 redactor 再复制
  - [x] SubTask 14.2: 在 `tests/test_trace_artifacts.py` 添加源文件含密钥时 artifact 已脱敏测试

- [x] Task 15: TraceStore run_id 路径校验 (P2-20)
  - [x] SubTask 15.1: 修改 `src/singularity/observability/store.py:36-39`，校验 `run_id` 不含 `..` 且非绝对路径
  - [x] SubTask 15.2: 在 `tests/test_trace_store.py` 添加路径逃逸拒绝测试

- [x] Task 16: SpanManager 线程安全 (P2-21)
  - [x] SubTask 16.1: 修改 `src/singularity/observability/spans.py`，`_stack` 改为 `threading.local()` 或加锁
  - [x] SubTask 16.2: 在 `tests/test_span_manager.py` 添加多线程并发 span 压栈/出栈测试

## Phase 4 — 文档与验证

- [x] Task 17: 更新 Runtime Flow Docs
  - [x] SubTask 17.1: 更新 `docs/architecture/modules/policy-approval-gates.md`，记录 grants 默认路径迁移、digest 校验、scope 收敛、去重维度变更
  - [x] SubTask 17.2: 更新 `docs/architecture/modules/tool-execution-runtime.md`，记录 grant 来源校验、缓存键增量失效、缓存/账本加锁
  - [x] SubTask 17.3: 更新 `docs/architecture/modules/context-compaction-observation-store.md`，记录脱敏覆盖扩展、ContextStore 并发
  - [x] SubTask 17.4: 更新 `docs/architecture/modules/trace-observation-audit-events.md`，记录 artifact 脱敏、run_id 校验、SpanManager 线程安全

- [x] Task 18: 运行验证
  - [x] SubTask 18.1: 运行 `python -m pytest tests --basetemp work/pytest-tmp-harden-review` 全量测试
  - [x] SubTask 18.2: 运行 `python -m ruff check .` 与 `python -m mypy`
  - [x] SubTask 18.3: 运行 `python scripts/verify_runtime_docs.py`
  - [x] SubTask 18.4: 运行 `python -m compileall -q src tests`

# Task Dependencies

- Task 1 (P0-4 parser) 无依赖，可立即开始
- Task 2 (P0-1 grants 路径) 与 Task 3 (P0-2 remote)、Task 4 (P0-3 去重) 都涉及 policy 层，建议顺序执行避免合并冲突
- Task 5 (P1-1 脱敏) 无依赖，可与 Phase 1 并行
- Task 6 (P1-2 沙箱) 与 Task 7 (P1-3 Docker) 都涉及 sandbox 层，建议顺序执行
- Task 8 (P1-4 资源边界) 与 Task 9 (P1-5 加锁) 都涉及 tools/executor.py，建议顺序执行
- Phase 3 (Task 10-16) 各项相互独立，可并行
- Task 17 (文档) 依赖 Phase 1-3 完成
- Task 18 (验证) 依赖所有代码与文档变更完成
