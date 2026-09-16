import assert from 'node:assert/strict'
import { test } from 'node:test'
import { readExecution } from '../src/execution'
import { cacheHitPercent, generationRate, sessionUsage, totalTokens } from '../src/sessionUsage'
import { event, observation, runtime, session, summary, usage } from './fixtures'
import type * as Wire from '../src/protocol.generated'

/** 冻结历史给 base，活动事件给增量；两者由服务端读盘冻结窗口保证不重叠。 */
const view = (base: Wire.SessionModelUsage, ...attempts: Array<Partial<Wire.RequestObservation>>) =>
  readExecution(session({
    history: { summary: summary({ usage: base }), turns: [], nextCursor: null },
    runtime: runtime(),
    activeEvents: attempts.map(attempt => event({ method: 'provider/attempt', params: { observation: observation(attempt) } })),
  }))

test('会话累计用量相加冻结历史与活动回合，没有用量时不显示', () => {
  const total = sessionUsage(view(
    { inputTokens: 1000, cachedInputTokens: 900, outputTokens: 50, generationMs: 2000, usagePresent: true, usageComplete: true },
    { status: 'ok', inputTokens: 200, cachedInputTokens: 100, outputTokens: 10, durationMs: 1000 },
  ))
  assert.ok(total)
  assert.equal(totalTokens(total), 1260)
  assert.equal(cacheHitPercent(total), 1000 / 1200 * 100)
  assert.equal(generationRate(total), 20, "输出 60 token ÷ 3 秒")
  assert.equal(sessionUsage(view(usage(), { status: 'started' })), null)
})

test('同一 requestId 的多次观测只取末次，未报告用量的请求让合计成为下界', () => {
  const started = { status: 'started' as const }
  const finished = { status: 'ok' as const, inputTokens: 200, cachedInputTokens: 0, outputTokens: 10, durationMs: 1000 }
  const total = sessionUsage(view(usage(), started, finished))
  assert.ok(total)
  assert.equal(total.inputTokens, 200, "started 观测不重复计入")
  assert.equal(total.usageComplete, true)
  assert.equal(sessionUsage(view(usage({ usagePresent: true, usageComplete: true }), started))?.usageComplete, false)
})
