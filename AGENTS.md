# Singularity 仓库指令

## 事实入口与范围

1. `.codex/repo-map.json` 存在且与当前 HEAD 一致时，可用其定位最小相关 Rust crate、符号和测试；否则以当前源码为准。
2. 先读实现、调用方、配置和相邻测试，再修改。不要把报告、旧文档或历史提交当作当前事实。
3. 默认不读取 `.git/`、`.singularity/`、`target/`、`work/` 或其他运行产物，除非任务明确涉及 Git、缓存、产物或环境诊断。
4. 不读取、输出或提交 `.env` 中的敏感值；环境检查只报告脱敏的 present/missing 状态。

## 命令与磁盘

1. Windows 上所有仓库命令必须通过 PowerShell 7（`pwsh.exe`）执行，不使用 Windows PowerShell 5.1。
2. Rust 构建和测试优先为单次命令设置 `CARGO_TARGET_DIR` 到空间充足的非系统盘，并复用同一目录；不要修改用户的全局 Cargo 配置。
3. 尽量不占用 C 盘。任务完成后，默认删除本次产生且可重建的 Cargo target、临时 evaluation、测试缓存、日志、临时工作树和一次性中间文件；用户明确要求保留时除外。
4. 删除或移动目录前先解析并校验绝对路径位于当前工作区或本次明确指定的临时目录。不得删除源码、用户数据、任务开始前已存在且归属不明的产物。
5. 最终回复说明已清理的产物，以及因交付或后续验证而保留的内容。

## 项目实现与目标仓库语言边界

1. Singularity 的核心产品运行时、公共协议、安全边界和发布二进制使用 Rust。允许为构建、测试、审计或维护引入职责明确的主流辅助工具，但不得形成第二套产品运行时、绕过 Rust 主链路或安全协议，也不得恢复 Python agent runtime、sidecar 或兼容入口。
2. 目标仓库可以使用 Python、Rust、Node.js、Go 或其他语言；命令工具应在严格 sandbox 中使用宿主机 `PATH` 已安装的工具链，不得把实现语言边界误作目标仓库能力限制。
3. `sg` 只通过 stdio JSON-RPC 调用 `singularity_app_server`；CLI 不直接依赖 agent、model、tools 或 store crate。
4. 当前工作树只保留当前真实结构。历史命名、schema、CLI、环境变量和迁移说明由 Git 历史保存，不新增兼容垫片、弃用别名、迁移读取入口或旧路径 re-export。
5. Evaluation 使用 `evaluation`、`eval`、`task`、`task set`、`runner`、`result`、`report` 等主流命名，不恢复迁移期自造分类。

## 运行时与安全

1. 主链路为 `sg -> AppServer -> AgentLoop -> ToolBroker -> WorkspaceTools -> SandboxBackend -> SessionStore`。
2. sandbox 保持 fail closed，并复用仓库内来自 Codex 的 Windows restricted-token、Job Object 和 elevated helper 实现。不得增加 local-process、no-sandbox 或 relaxed fallback。
3. `workspace-write` 下的命令必须在严格 sandbox 内执行；网络默认拒绝。权限、approval、protected path、cwd canonicalization 和越界写入检查不得弱化。
4. 取消必须传播到 provider 和在途 sandbox command；取消请求之后的晚到 completion、assistant item 或 terminal trace 不得覆盖 interrupted 状态。
5. provider 原始响应、prompt、tool raw arguments、环境变量、密钥和内部 audit metadata 不得进入公共 CLI、model tool payload 或未脱敏 trace。

## 文档

1. `docs/singularity.md` 是唯一架构事实文档，只描述当前核心产品运行时中的 crate 边界、对象、调用链、持久化和失败路径。
2. 主链路、协议、状态映射、sandbox、approval、provider、evaluation、trace 或 store 变化时，同步更新 `docs/singularity.md` 的相关部分。
3. 不恢复 `docs/architecture/modules/`、迁移报告、阶段报告、旧路线图或 Python 时代文档。

## 验证

测试与验证

1. 采用风险驱动测试（risk-based testing）和测试影响分析（test impact analysis）。根据改动文件、调用方、公共接口、数据流和信任边界选择最小充分的验证范围，不默认新增测试，也不默认执行整个 workspace。

2. 只有以下情况才新增或实质扩展测试：

   - 修复可复现缺陷，需要能够在修复前失败、修复后通过的回归测试；
   - 新增或改变外部可观察行为、公共协议、持久化 schema、权限、approval、sandbox、安全边界、并发、取消、恢复或错误归约；
   - 现有测试无法覆盖可能造成真实回归的关键成功路径或失败路径。

3. 不得仅为提高覆盖率、展示工作量或形式完整而新增测试。纯重命名、文件移动、格式调整、注释或文档修改、无行为变化的内部重构，以及已被现有测试充分覆盖的改动，默认不新增测试。

4. 不测试无意义的实现细节，例如简单 getter、常量转发、字段逐项复制、私有函数调用方式或与业务行为无关的内部结构。优先验证公共契约和外部可观察结果。

5. 新增测试前先检查最小相关测试范围，确认：

   - 哪个具体行为可能回归；
   - 现有测试为什么不能覆盖；
   - 新测试能够捕获什么真实缺陷。

   无法明确回答时，不新增测试。

6. 优先复用现有 fixture、helper 和测试模块。相同输入结构或边界条件优先使用表驱动测试（table-driven test），不要跨单元测试、集成测试和端到端测试重复验证同一行为。不得为了一个局部行为启动不必要的完整 AgentLoop、AppServer、真实 provider 或操作系统级 sandbox。

7. 不要无条件向超大测试文件继续追加内容。测试应放入最小相关的行为模块；只有在能够明显降低后续阅读、定位和维护成本时，才拆分现有测试文件，不进行纯形式化拆分。

8. 验证按成本从低到高执行：

   - 仅文档、注释或非运行时文本变化：运行 "git diff --check"；
   - 单个 crate 的局部实现变化：运行相关格式检查、该 crate 的 "cargo check" 和直接相关测试；
   - 公共类型、跨 crate 接口或共享基础设施变化：检查受影响 crate，并运行相关调用链测试；
   - workspace 配置、依赖、工具链、公共协议、持久化、安全边界或跨模块主链路变化：运行完整 workspace 检查；
   - 发布准备、CI 收口或用户明确要求时：运行全部格式、check、clippy 和测试。

9. 真实 provider evaluation 仅在改动实质影响 AgentLoop、模型调用、工具执行、sandbox、approval、completion 或 evaluation 的端到端能力，且静态检查和定向测试不足以证明行为时执行。一次任务最终状态未变化时不得重复运行。

10. 已成功且输入未变化的检查不得重复执行。测试失败时先判断是产品回归、环境问题还是脆弱或过期测试，不得为了迎合错误测试而扭曲运行时代码，也不得通过新增重复测试掩盖根因。

11. 不弱化 CI 的完整 workspace 和跨平台门禁。任务结束时思考新增或修改的测试、每项测试的必要性、实际执行的检查，以及未执行检查的原因；未运行完整验证时不得声称全部验证通过。
## Git

1. 修改前确认仓库、分支、工作树和用户未提交改动；不覆盖无关内容。
2. 验证通过后创建范围单一、信息明确的本地提交。
3. 未经用户明确要求不得 push、merge、rebase、reset、删除 stash 或改写历史。

## Agent skills

### Issue tracker

任务和需求记录在 GitHub Issues。详见 `docs/agents/issue-tracker.md`。

### Triage labels

使用默认的五类 triage 标签。详见 `docs/agents/triage-labels.md`。

### Domain docs

单上下文（single-context）布局；现有仓库指令和当前架构文档优先。详见 `docs/agents/domain.md`。
