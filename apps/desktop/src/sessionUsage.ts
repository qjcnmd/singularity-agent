import type { SessionView } from './execution'
import type { SessionModelUsage } from './protocol'

/**
 * 汇总会话累计模型用量与延迟指标：
 * 将服务端提供的历史统计与当前进行中回合的实时请求观测累加。
 * 若无有效用量或 TTFT 样本则返回 null。
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

/** 缓存命中率（cached_tokens / input_tokens），无输入时返回 null */
export function cacheHitPercent(usage: SessionModelUsage): number | null {
  return !usage.cacheUsageComplete || usage.inputTokens === 0 ? null : usage.cachedInputTokens / usage.inputTokens * 100
}

/** 平均输出速率 TPS（仅计算有解码计时的请求，排除首 Token 等待时间） */
export function generationRate(usage: SessionModelUsage): number | null {
  return usage.decodeMs === 0 ? null : usage.decodeTokens / (usage.decodeMs / 1000)
}

/** 首 Token 平均延迟 TTFT（ms） */
export function firstTokenLatency(usage: SessionModelUsage): number | null {
  return usage.ttftRequests === 0 ? null : usage.ttftMs / usage.ttftRequests
}
