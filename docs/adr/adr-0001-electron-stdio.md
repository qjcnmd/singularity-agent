# Electron 桌面与 Rust AppServer 使用 stdio 通信

日期：2026-09-25。状态：已采用。

Singularity 保留现有 React/Vite、CSS、Canvas/Motion 与交互逻辑，以 Electron 作为 Windows 日常入口。桌面不监听 localhost HTTP 或 WebSocket；Rust 继续拥有模型配置、会话、Agent 执行和持久事实。

桌面位于 `apps/desktop`，Rust 应用层位于 `crates/app`。隔离的 preload 只公开 RPC、连接和事件订阅，主进程校验自有主 frame，再通过子进程 stdin/stdout 交换逐行 JSON。请求关联 ID 是传输元数据；方法、参数、结果、错误及流事件继续使用 `singularity_protocol`，不复制业务协议。原生文件夹选择由 Electron 实现既有 `directory.pick` 合同。

Vite production 资源通过应用内 `singularity://app/` 协议读取，该协议没有网络监听。保留 Chromium 沙箱、context isolation 与 CSP，渲染进程不能直接使用 Node。窗口关闭隐藏到托盘，刷新不影响 Rust；托盘退出关闭 stdin，Rust 取消任务并等待结算。十秒内未退出时主进程终止后端，未完成记录使用现有历史恢复机制。意外后端退出显示错误，不自动重发 RPC。

选择 stdio 是因为现有 AppServer 已与 HTTP 适配分离，而且只需要一个桌面客户端。管道由父进程创建，不用管理命名管道地址、端口或连接鉴权。代价是维护 Electron 主进程与 Rust 子进程的生命周期，以及比单一 Rust 可执行文件更大的发布目录。

未采用 Rust Node 原生插件：会引入 ABI、原生插件构建和进程内故障耦合，当前没有避免 JSON 序列化的性能依据。未采用独立命名管道服务：当前不需要跨进程发现、多个外部客户端或后端独立常驻；需求变化时重新评估。

配置与会话数据继续使用 `SINGULARITY_HOME`。Electron 视图存储按数据目录隔离，原浏览器 localStorage 不自动迁移，升级前需要自行保存尚未发送的浏览器草稿。
