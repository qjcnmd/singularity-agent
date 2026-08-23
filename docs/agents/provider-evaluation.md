# Provider 与 Evaluation

仅在涉及 Provider、模型、协议、真实调用或评估时读取。

- Provider 行为以当前配置、实际 API 合同和 wire 证据为准；静态模型声明、用户配置、effective 配置和实际 payload 是不同事实。
- Retry 只处理可明确重试的同一请求失败；不得换模型、静默 fallback、重采样或吞错制造成功。
- Provider 调用、上下文组装、Compaction、工具执行/回放、Agent 循环和客户端链路的行为验证必须使用真实模型调用；mock/fake 仅用于纯逻辑单元测试。
- 用户级 Provider 配置持久化在默认 Singularity home：`config.json` 与唯一的 `auth.v1.json`。真实调用需要认证时，先核对这两个文件和脱敏后的配置诊断；不得因当前进程没有 API key 环境变量而判定凭据不存在，也不得读取、打印或手工转录密钥。
- 真实调用使用临时目录、临时 SINGULARITY_HOME 和配置副本，并保存可复核证据。核心链路优先运行 `crates/cli/tests/core_chain_smoke.rs` 中对应的 ignored smoke；其 fixture 会把所选持久化配置和认证文件复制到临时 home。只有 fixture 的明确诊断证明持久化配置或认证缺失/无效，或者真实请求返回可定位的外部阻断时，才能把该项报告为未执行或阻断。
- Evaluation 先单 cell 冒烟再小批量；分别记录产物/checker、turn、Provider/transport 和评估进程。失败先排除题目、wire、transport、Agent、checker 和环境解释，不能先归因模型能力。
