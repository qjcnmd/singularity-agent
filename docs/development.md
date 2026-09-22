# 开发与验证

安装依赖和构建发布程序见 [安装与运行](INSTALL.md)。产品方向见 [宪章](constitution.md)，模块关系、运行流程与源码入口见 [架构图谱](architecture.md)。

## 本地运行

在仓库根目录生成前端资源，再构建或启动 Rust 程序：

```powershell
npm --prefix crates/cli/web ci
npm --prefix crates/cli/web run build
cargo run -p singularity_cli --locked -- --no-open --port 3080
```

打开终端打印的当前进程启动链接。前端资源嵌入可执行文件；修改页面后需重新生成前端资源并构建 Rust 程序，刷新旧进程无法加载新资源。端口已占用时先确认占用进程与数据目录，再按下面的更新或隔离验证方式启动。

更新用户正在使用的工作台时，沿用当前数据目录与配置，停止使用该目录的旧进程后启动新版；使用自定义目录时保持原来的 `SINGULARITY_HOME`。交付环境的选择遵循[项目指令](../AGENTS.md#验证与交付)。只改文档无需重建或重启工作台。

Windows 会锁定正在运行的可执行文件。需要保留现有开发实例进行隔离验证时，可使用独立构建 profile，并由系统分配空闲端口：

```powershell
cargo run -p singularity_cli --profile preview --config 'profile.preview.inherits="dev"' --locked -- --no-open --port 0
```

执行该命令前，为验证进程设置独立的 `SINGULARITY_HOME`；该目录不会自动加载原目录中的模型配置与历史。命令沿用 dev 配置，把产物放在 Cargo 输出目录的 `preview/` 下，不覆盖运行中的 `debug/singularity.exe`。改端口或构建目录不能绕过数据目录单实例限制。

评估入口复用同一 Agent：

```powershell
cargo run -p singularity_cli --locked -- --json "summarize this repository"
```

该命令会调用已配置模型，每次创建并保存新会话。评估器通过 `SINGULARITY_HOME` 隔离配置与会话，具体配置见安装说明。

页面状态回归可使用本地模拟 Provider，在独立数据目录中验证触发、流式更新、完成、刷新和再次使用；这不替代真实模型验证。模型选择及调用范围按项目指令执行，临时提供商、会话和进程在验证后清理。

## 检查

检查范围按 [项目指令](../AGENTS.md#验证与交付) 选择，日常先运行覆盖本次修改的最小测试集合。Rust 用包名和测试名过滤，例如在仓库根目录运行：

```powershell
cargo test -p singularity_runtime --lib --locked operation_start_is_durable_before_the_provider_call_and_terminal_after
```

将示例中的包名和过滤条件换成受影响的行为，确认实际选中了用例。跨模块修改选择相关边界测试；只有失败、遗漏路径或共享机制变化带来具体疑点时扩大范围。只改测试时验证改动后的用例及承接覆盖的用例；不因此重跑无关模块、重建 production 页面或调用模型。普通文档检查最终内容、链接与 `git diff --check`。CI 和发布步骤由 `.github/workflows` 维护，不作为日常修改的默认验证清单。

## 协议更新

Rust 的 `protocol` crate 维护 RPC 方法与 DTO。修改协议后，在仓库根目录运行以下命令更新客户端声明，再检查生成 diff：

```powershell
cargo run -p singularity_protocol --features typescript --example export_types
cargo test -p singularity_protocol --features typescript --locked
```

序列化 fixture 位于 `crates/protocol/tests/fixtures/`，覆盖事件、流信封和 RPC 响应。仅在有意改变相应合同后，设置 `UPDATE_PROTOCOL_FIXTURES=1` 运行协议测试并检查 JSON 差异；普通测试只核对 fixture。协议测试同时校验生成的 TypeScript 声明与 Rust 合同逐字节一致，消费前端不再另设测试。

## 测试保留与删减

保留跨模块的调用链测试：它们从外层入口一路走到持久化事实，例如 `crates/runtime/src/tests/` 的崩溃恢复与控制流、`crates/cli/src/tests/` 的无交互执行、`crates/cli/src/web/app_server/tests/` 中按主题组织的 RPC 端点，以及 `crates/protocol/tests/` 的跨端字节合同。这些用例随真实行为变化而失败，长期价值高。

测试分为长期保留与临时验证。开发、排错和验证时允许编写只验证单个函数、私有结构或内部字段的细粒度单元测试等临时测试；它们通过并完成验证用途后，必须在交付前清理，连同失去用途的夹具、脚本和配置一并移除。长期保留按以下标准判断：

- 长期测试应能说明一个当前行为约定或具体故障，并提供已有用例没有覆盖的保障。例如崩溃后恢复收敛、停止后不重放工具、会话损坏时明确失败。
- 同一场景优先在已有调用链测试中补充必要断言；不按每次修改、每个函数或每个文件自动新增。
- 只为一次调查服务的探针、打印、采样和临时环境检查，调查结束后清理。使用临时目录或假模型只是运行方式，不决定测试是否有长期价值。
- 真实持久格式和跨端协议属于使用契约，其测试不能仅因包含 JSON 字段而被删除。

## 测试组织

采用 [Rust 的测试组织约定](https://doc.rust-lang.org/book/ch11-03-test-organization.html)：只使用 crate 公共接口的独立集成测试放在 crate 根下的 `tests/`，跨模块但需要内部接口的行为测试集中在各 crate 的 `src/tests/`。

- Runtime 与 CLI 中涉及多个模块的行为测试集中在各自的 `src/tests/`。
- 协议的外部契约测试使用 `crates/protocol/tests/`，由 Cargo 自动发现；不把内部模块伪装成此类 target。
- 跨 crate 使用的测试夹具留在拥有相应能力的模块，由 `test-support` feature 开启。只供单个测试组使用的辅助代码与该组放在一起，不增加全仓测试工具包。
- 临时单元测试可放在所属模块，验证完成后按上一节要求清理；长期测试按行为或契约归入对应测试组。

## CI 与发布

[CI 入口](../.github/workflows/ci.yml) 在推送 `main` 时调用 [共享检查工作流](../.github/workflows/rust-gates.yml)。Windows 任务执行前端构建、Rust 格式、Clippy、测试和二进制构建；Ubuntu 任务只运行 cargo-deny 与前端生产依赖审计，不编译或验证 Linux 产品。依赖检查复用同一锁文件与策略，保留 Ubuntu 执行器不代表支持 Linux。工具版本和具体步骤由工作流维护。

需要在本地复现完整功能检查时，在安装依赖后执行：

```powershell
npm --prefix crates/cli/web run build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked --no-deps -- -D warnings
cargo test --workspace --all-targets --features singularity_protocol/typescript --locked --no-fail-fast
cargo build --workspace --bins --locked
git diff --check
```

这组命令不包含独立依赖审计、浏览器交互或真实模型验证；按修改范围选择相应检查，不把完整集合用于每次修改。

[发布工作流](../.github/workflows/release.yml) 先复用检查，再构建 Windows x86-64 release 程序。打包脚本从 `cargo metadata.target_directory` 查找产物；归档包含可执行文件、README、LICENSE 和 INSTALL，另生成 SHA256 校验和。推送 `v*` 标签会发布 GitHub Release；手动运行只生成工作流产物。源码构建命令由 [安装说明](INSTALL.md#从源码构建) 维护。

## 可选评估工具

本机评估器位于 `C:/Users/Lenovo/Desktop/Singularity-Evaluator`，通过 `singularity --json` 在隔离工作区运行任务。任务、checker、参数和配置由评估器仓库维护。

是否使用见 [项目指令](../AGENTS.md#验证与交付)。模型、任务和预算按本次明确要求选择。
