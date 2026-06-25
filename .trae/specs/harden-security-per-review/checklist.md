# Checklist

## Phase 1 — P0 阻塞上线

- [x] P0-4: `verification/parsers.py` 的 `NpmBuildFailureParser.parse` 使用 `group(3)` 提取行号，不再 `int(group(2))`
- [x] P0-4: 存在 npm 构建错误行解析测试，断言 `line == 12` 且不抛 `ValueError`
- [x] P0-1: `_approval_grants_path` 默认返回 `~/.singularity/policy/approval_grants.jsonl`，非工作区内
- [x] P0-1: audit 日志默认路径同样移出工作区
- [x] P0-1: PolicyEngine 命令通道对工作区内 `.singularity/policy/` 写操作 hard-deny
- [x] P0-1: ToolExecutor 消费 grant 前校验来源可信（工作区外或签名验证）
- [x] P0-1: 测试验证模型 shell 写 `.singularity/policy/` 被拒绝
- [x] P0-1: 测试验证不可信 grant 不被消费
- [x] P0-2: `import_grant` 校验 `request_digest` 与重算摘要一致
- [x] P0-2: `import_grant` 校验 `grant.scope ⊆ decision.required_approval.scope`
- [x] P0-2: `export_request` 导出 `decision.required_approval.scope` 供导入侧校验
- [x] P0-2: 测试验证伪造 scope 全开 grant 被拒绝
- [x] P0-2: 测试验证 digest 篡改被拒绝
- [x] P0-3: `ApprovalGrant.from_dict` 缺 `grant_id` 时生成确定性 ID（基于 decision_id+request_id+approved_by 的 hash）
- [x] P0-3: `register_grant` 按 `decision_id` 去重，同一决策仅一个活跃 grant
- [x] P0-3: 测试验证反复 import 同一 grant 不放大为多个可消费 grant

## Phase 2 — P1 高优先级

- [x] P1-1: `SECRET_PATTERNS` 新增 AWS Access Key (`AKIA...`)、JWT (`eyJ...`)、Slack (`xoxb-`)、Stripe (`sk_live_`)、Google (`AIza`)
- [x] P1-1: `redact_value` dict 分支补全 `authorization`/`credential`/`passphrase`/`private_key`/`access_token`/`refresh_token`/`client_secret` 字段名
- [x] P1-1: `add_tool_result` 对所有 sensitivity 级别统一过 redactor
- [x] P1-1: `observability/redaction.py` 的 token 正则同步更新
- [x] P1-1: 测试覆盖 AWS/JWT/Slack/Stripe/Google 密钥脱敏
- [x] P1-2: `read_only`/`readonly` 不再映射为 `COPY_ON_WRITE_WORKSPACE`，映射为真正只读
- [x] P1-2: `LocalStagingBackend.capabilities()` 的 `filesystem_isolation` 降级为 `False`
- [x] P1-2: `network.mode=DENIED` 在无网络隔离后端上产生 `SandboxViolation`（fail-closed）
- [x] P1-2: `capability_summary` 基于选中后端而非全部后端并集
- [x] P1-2: 测试覆盖 read-only 真正只读、network DENIED fail-closed
- [x] P1-3: Docker 后端添加 `--user`/`--cap-drop=ALL`/`--security-opt no-new-privileges`/`--init`
- [x] P1-3: 支持镜像 digest pinning
- [x] P1-3: 添加 `--memory`/`--pids-limit` 资源上限
- [x] P1-3: 超时后调用 `docker stop` 清理孤儿容器
- [x] P1-3: 测试验证加固参数构造
- [x] P1-4: `read_file` 读取前检查文件大小，超限截断
- [x] P1-4: `search_text` 限制单文件扫描大小
- [x] P1-4: 只读工具缓存键改为基于 `invalidate_paths` 的增量失效，移除全工作区 sha256 哈希
- [x] P1-4: 写工具执行后只失效受影响路径的缓存条目，而非 `self._cache.clear()`
- [x] P1-4: 测试覆盖大文件截断、缓存增量失效
- [x] P1-5: `ToolExecutor._cache`/`_ledger` 加 `threading.RLock`，所有访问点持锁
- [x] P1-5: 测试验证多线程并发访问不抛异常

## Phase 3 — P2 关键项

- [x] P2-10: `ContextStore` SQLite 连接加 `check_same_thread=False`，读路径持锁
- [x] P2-10: 测试覆盖并发读写
- [x] P2-11: `PlannerStore._write_json` 改原子写入（tempfile+os.replace+fsync）
- [x] P2-11: `append_event` 加文件锁
- [x] P2-11: 测试覆盖原子性与并发追加
- [x] P2-17: `WorkspaceMutationManager` 五个容器加 LRU 上限
- [x] P2-17: journal `.before` 制品成功提交后清理
- [x] P2-17: 测试覆盖无界增长防护
- [x] P2-18: live eval patch diff 跳过 `.env`/`*.pem`/`*.key` 或强制 redact
- [x] P2-18: 测试验证 `.env` 内容不泄入 `result.json`
- [x] P2-19: `register_file_artifact(sensitive=False)` 对源文件过 redactor
- [x] P2-19: 测试验证源文件含密钥时 artifact 已脱敏
- [x] P2-20: `TraceStore` 校验 `run_id` 不含 `..` 且非绝对路径
- [x] P2-20: 测试覆盖路径逃逸拒绝
- [x] P2-21: `SpanManager._stack` 改线程局部存储或加锁
- [x] P2-21: 测试覆盖多线程并发 span 压栈/出栈

## Phase 4 — 文档与验证

- [x] `docs/architecture/modules/policy-approval-gates.md` 更新 grants 路径迁移、digest 校验、scope 收敛、去重维度变更
- [x] `docs/architecture/modules/tool-execution-runtime.md` 更新 grant 来源校验、缓存键增量失效、缓存/账本加锁
- [x] `docs/architecture/modules/context-compaction-observation-store.md` 更新脱敏覆盖扩展、ContextStore 并发
- [x] `docs/architecture/modules/trace-observation-audit-events.md` 更新 artifact 脱敏、run_id 校验、SpanManager 线程安全
- [x] `python -m pytest tests` 全量通过
- [x] `python -m ruff check .` 通过
- [x] `python -m mypy` 通过
- [x] `python scripts/verify_runtime_docs.py` 通过
- [x] `python -m compileall -q src tests` 通过
