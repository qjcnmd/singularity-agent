# 开发与验证

安装依赖和构建发布程序见 [安装与运行](INSTALL.md)。产品方向见 [宪章](constitution.md)，运行契约与模块职责见 [架构说明](singularity.md)。

## 本地运行

在仓库根目录生成前端资源，再构建或启动 Rust 程序：

```powershell
npm --prefix crates/cli/web ci
npm --prefix crates/cli/web run build
cargo run -p singularity_cli --locked -- --no-open --port 3080
```

打开终端打印的当前进程启动链接。前端资源嵌入可执行文件；修改页面后需重新生成前端资源并构建 Rust 程序，刷新旧进程无法加载新资源。端口已占用时选择另一个端口，保留已有实例。

无交互入口复用同一 Agent：

```powershell
cargo run -p singularity_cli --locked -- --print "summarize this repository"
cargo run -p singularity_cli --locked -- --json "summarize this repository"
```

这些命令会调用已配置模型。自动化和测试可通过 `SINGULARITY_HOME` 隔离配置与会话，具体配置见安装说明。

## 检查

检查范围按 [项目指令](../AGENTS.md#验证与交付) 选择，日常先运行覆盖本次修改的最小测试集合。Rust 用包名和测试名过滤，例如在仓库根目录运行：

```powershell
cargo test -p singularity_agent --lib --locked default_model_setup_replays_continuation_through_tools_and_reopen
```

前端可在 `crates/cli/web` 目录按用例名过滤：

```powershell
node --experimental-transform-types --import ./tests/register-typescript.mjs --test --test-name-pattern='individual tools preserve|request lookup and prompt head' tests/workbench.test.mjs
```

将示例中的包名和过滤条件换成受影响的行为，确认实际选中了用例。跨模块修改选择相关边界测试；只有失败、遗漏路径或共享机制变化带来具体疑点时扩大范围。只改测试时验证改动后的用例及承接覆盖的用例；不因此重跑无关模块、重建 production 页面或调用模型。普通文档检查最终内容、链接与 `git diff --check`。CI 和发布步骤由 `.github/workflows` 维护，不作为日常修改的默认验证清单。

## 测试保留与删减

[Cargo 的贡献指南](https://doc.crates.io/contrib/tests/writing.html)通过实际命令、文件和输出验证行为；[Testing Library](https://testing-library.com/docs/guiding-principles/)强调按使用方式验证；[Google 的测试实践](https://testing.googleblog.com/2015/01/testing-on-toilet-change-detector-tests.html)指出，仅复写实现、随内部结构一起变化的测试会增加维护成本。这里采用这些原则，不照搬大型项目的用例数量、覆盖率目标或测试设施。

- 长期测试应能说明一个当前行为约定或具体故障，并提供已有用例没有覆盖的保障。例如刷新后消息保留、停止后不重放工具、会话损坏时明确失败。已发生故障的最小复现可以保留为回归测试。
- 同一场景优先在已有行为测试中补充必要断言，删除被覆盖的旧用例及专用辅助代码。测试不按每次修改、每个函数或每个文件自动新增。
- 只为一次调查服务的探针、打印、采样和临时环境检查，调查结束后清理。使用临时目录或假模型只是运行方式，不决定测试是否有长期价值。
- 只证明字段照搬、内部编号格式、私有初始化结构或测试自己构造的数据的检查，应删除或改成行为检查。真实持久格式和跨端协议属于使用契约，其测试不能仅因包含 JSON 字段而归入此类。
- 用例按相关功能组织。删减以减少重复保障和维护工作为准，不以合并文件、减少测试计数或达到固定代码比例代替判断。

## 测试组织

采用 [Rust 的测试组织约定](https://doc.rust-lang.org/book/ch11-03-test-organization.html)：内部测试留在 `src`，仅使用 crate 公共接口的独立集成测试放在 crate 根下的 `tests/`。`#[cfg(test)]` 控制内部测试的编译，它们不会进入正常发布程序。

- 小型模块测试可直接放在所属源码的 `mod tests` 中；较长用例使用所属模块内的 `tests.rs` 或相邻的 `*_tests.rs`，保持职责就近，不为文件数量统一搬动。
- Runtime 与 CLI 中涉及多个模块且使用内部接口的行为测试集中在各自的 `src/tests/`。协议的外部契约测试继续使用 `crates/protocol/tests/`，由 Cargo 自动发现；不把内部模块伪装成此类 target。
- 跨 crate 使用的测试夹具留在拥有相应能力的模块，由 `test-support` feature 开启。只供单个测试组使用的辅助代码与该组放在一起，不增加全仓测试工具包。
- 前端测试留在 `crates/cli/web/tests/`，与实际 TypeScript 模块和 Node 依赖一起维护。它们验证状态与投影；页面操作或布局变化仍按项目指令在实际页面验证。

## 可选评估工具

本机评估器位于 `C:/Users/Lenovo/Desktop/Singularity-Evaluator`，通过 `singularity --json` 在隔离工作区运行任务。任务、checker、参数和配置由评估器仓库维护。

是否使用见 [项目指令](../AGENTS.md#验证与交付)。模型、任务和预算按本次明确要求选择。
