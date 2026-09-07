# Singularity 上下文工作集实验

基线：`b031ff411b334ccae72a03e9688a551ba680a0aa`。设计日期：2026-09-07。

**本目录是完整目标设计 + 独立可执行的策略参考，不是已经接入正式 Rust Agent 的功能。**
`singularity --json`、WebUI、Session v5 和现有 Summary 在本分支均未改变。原型只运行本地确定性实验，不调用模型，不执行任务工具，不消费 API 额度。真实模型实验和正式接线见 [INTEGRATION.md](INTEGRATION.md)。

## 收敛后的两个候选

| 候选 | 继续执行主要依赖什么 | 预期收益与代价 |
|---|---|---|
| **State Workspace / 状态工作集（首选）** | 持续更新、带来源的工作状态 + 当前必要原文 + 尚未处理的观察 | 减少反复发送过程；必须验证模型维护状态是否可靠 |
| **Evidence Workspace / 原文工作集** | 可替换的对象槽位、原文引用、短接续历史；不要求每步写语义状态 | 减少改写失真；通常需要更多原文和模型当步整合 |

这是两种针对同一开发工作流的完整设计，不是按论文分别克隆产品。两者共享源存储、执行事实、原文访问、预算和缓存周期。真正比较的是：**已经处理的信息，主要由显式工作状态承接，还是由有限原文工作集承接。**

立即/批量刷新属于同一控制器的评测参数，不再单独包装成第三种方案。笔记重置、神经记忆模拟、向量库和全局语义图均不作为额外必选架构。

## 阅读与运行

- [DESIGN.md](DESIGN.md)：综合设计、淘汰理由、更新时序、增长分析和初步判断。
- [INTEGRATION.md](INTEGRATION.md)：Rust 对象草图、现有调用点、Session/UI/协议/恢复接线。
- [EVALUATION.md](EVALUATION.md)：隔离变量、真实开发测试、判定规则。
- [CODEX.md](CODEX.md)：给 Codex 的执行入口与交付边界。
- `engine.py`：两策略共用的可执行机制参考。
- `test_engine.py`：机制合同测试。
- `benchmark.py`、`mechanics-results.json`：确定性模拟与本次运行结果。

在仓库根目录执行（Python 3.10+，无需第三方依赖）：

```sh
python -m unittest discover -s experiments/context-workspace -p "test_*.py" -v
python experiments/context-workspace/benchmark.py --steps 120
```

PowerShell 也可直接执行上述命令。输出单位是 **UTF-8 字节，不是 tokens**。公共前缀字节也不等于供应商缓存命中。基线字段里的 unbounded replay 仅供量级校验，**不代表现有 Summary**。

## 已完成与未完成

已完成：两个策略的局部状态更新、来源验证、原文窗口、预算/增量重建、持久请求视图、原文回读与召回去重的独立原型；23 项测试通过；两种 120 步模拟已执行。

未完成：正式 Rust 接线、真正的 tokenizer/usage/cache 适配、FTS5 索引、原生工具与 reasoning replay、WebUI 开关、跨进程恢复、真实模型任务比较。不能把原型的 `Journal` 替代正式 `SessionManager`：它刻意没有跨进程锁，输入也已经是闭合的协议单元。

这里没有“已证明优于 Summary”的结论。参考实现用于检查设计是否自洽，Codex 需要先审查并接入真实执行链，才能测实际成功率。
