import type { SessionView } from './execution'
import type { SessionModelUsage } from './protocol'

/** 后端统一累计历史和活动请求；没有用量或计时样本时不显示统计。 */
export function sessionUsage(session: SessionView | null): SessionModelUsage | null {
  if (session === null) return null
  const usage = session.runtime.usage ?? session.summary.usage
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
