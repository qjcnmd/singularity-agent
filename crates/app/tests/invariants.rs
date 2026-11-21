//! Singularity 后端黑盒不变量集成测试。
//!
//! 保留标准：只从进程外（真实 `singularity.exe` 的 `stdin/stdout` JSON-RPC、临时数据目录、
//! 本地 HTTP 协议回放桩与测试 MCP stdio 脚本）验证跨模块契约与系统级不变量；
//! 不锁死内部实现细节、具体策略阈值或参数常数。

#[path = "invariants/harness.rs"]
mod harness;
#[path = "invariants/scenarios_core.rs"]
mod scenarios_core;
#[path = "invariants/scenarios_resilience.rs"]
mod scenarios_resilience;
#[path = "invariants/scenarios_runtime.rs"]
mod scenarios_runtime;
