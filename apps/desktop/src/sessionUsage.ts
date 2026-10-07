import type { SessionView } from './execution'
import type { SessionModelUsage } from './protocol'

/**
 * 会话累计用量 = 服务端按整份账本聚合的冻结历史 + 当前回合的实时观测。
 *
 * 两部分不重叠由服务端的读盘冻结窗口保证：回合开始时冻结的快照不含该回合之后
 * 落盘的记录，该回合的观测只随活动事件到达，结算后的下一次读取才把它并入快照。
 * 因此这里直接相加即可：既不用在前端重算历史（历史分页加载，重算会漏掉更早的
 * 回合），也不会重复计数。归并规则与服务端 `session_usage` 一致：同一 requestId
 * 取末次观测——活动事实本就以 requestId 为 id 做 upsert（`execution.ts`），因此
 * 每个请求在这里只出现一次；TTFT 只统计实测样本，消费只统计上报 usage 的请求。
 * 没有消费记录的请求不进入消费计数，也不影响缓存完整性。
 * 没有消费或首 token 计时样本时返回 null，调用方不显示伪造的零消费。
 */
export function sessionUsage(session: SessionView | null): SessionModelUsage | null {
  if (session === null) return null
  const usage = { ...session.summary.usage }
  for (const turn of session.facts.active) {
    for (const item of turn.items) {
      if (item.kind !== 'request') continue
      const observation = item.observation
      if (observation.ttftMs !== undefined) {
        usage.ttftMs += observation.ttftMs
        usage.ttftRequests += 1
      }
      if (observation.inputTokens === null && observation.outputTokens === null) continue
      usage.inputTokens += observation.inputTokens ?? 0
      usage.cachedInputTokens += observation.cachedInputTokens ?? 0
      usage.outputTokens += observation.outputTokens ?? 0
      usage.totalTokens += observation.totalTokens!
      usage.cacheUsageComplete &&= observation.cachedInputTokens !== null
      if (observation.decodeMs !== undefined && observation.decodeMs > 0 && observation.outputTokens !== null) {
        usage.decodeMs += observation.decodeMs
        usage.decodeTokens += observation.outputTokens
      }
      usage.generationMs += observation.durationMs
      usage.usagePresent = true
    }
  }
  return usage.usagePresent || usage.ttftRequests > 0 ? usage : null
}

/** 缓存命中率：分母是输入（不含输出）；没有输入时无定义。 */
export function cacheHitPercent(usage: SessionModelUsage): number | null {
  return !usage.cacheUsageComplete || usage.inputTokens === 0 ? null : usage.cachedInputTokens / usage.inputTokens * 100
}

/** 平均 TPS 只统计同时有输出计数和生成耗时的请求，排除首个 token 的等待时间。 */
export function generationRate(usage: SessionModelUsage): number | null {
  return usage.decodeMs === 0 ? null : usage.decodeTokens / (usage.decodeMs / 1000)
}

/** 首 token 平均延迟只由有计时记录的请求计算，未知样本不计入分母。 */
export function firstTokenLatency(usage: SessionModelUsage): number | null {
  return usage.ttftRequests === 0 ? null : usage.ttftMs / usage.ttftRequests
}
