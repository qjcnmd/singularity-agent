# Provider 与 Evaluation

仅在涉及 Provider、模型、协议、真实调用或评估时读取。

- Provider 行为以当前配置、实际 API 合同和 wire 证据为准；静态模型声明、用户配置、effective 配置和实际 payload 是不同事实。
- Retry 只处理可明确重试的同一请求失败；不得换模型、静默 fallback、重采样或吞错制造成功。
- Provider 调用、上下文组装、Compaction、工具执行/回放、Agent 循环和客户端链路的行为验证必须使用真实模型调用；mock/fake 仅用于纯逻辑单元测试。
- 真实调用使用临时目录、临时 SINGULARITY_HOME/DB 和配置副本，并保存可复核证据。
- Evaluation 先单 cell 冒烟再小批量；分别记录产物/checker、turn、Provider/transport 和评估进程。失败先排除题目、wire、transport、Agent、checker 和环境解释，不能先归因模型能力。
