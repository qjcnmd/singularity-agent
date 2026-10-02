import { join, resolve } from 'node:path'
import { mkdirSync, writeFileSync } from 'node:fs'
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { setupE2E, rpc, modelSelector } from './support.mjs'

// 统计条行为回归：按提供方实际用量决定缓存命中率是否可显示，
// 运行和取消不把未上报的缓存明细伪装为零命中。
const { output, launch } = setupE2E('e2e-stats-bar')
const app = await launch()
const errors = []
const report = { command: 'node apps/desktop/e2e/stats-bar.mjs', model: modelSelector, executable: process.env.SINGULARITY_E2E_PACKAGED }
let server
try {
  const page = await app.firstWindow()
  page.on('pageerror', error => errors.push(error.message))
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  await page.waitForSelector('.app-shell', { timeout: 30_000 })

  // 项目通过 RPC 准备；统计从新建任务、输入发送和流式响应的真实 UI 路径验证。
  const workspaceDir = resolve(join(output, 'workspace'))
  mkdirSync(workspaceDir, { recursive: true })
  const workspaceEntry = (await rpc(page, 'app.bootstrap')).workspaces.find(item => item.root.replaceAll('\\', '/').replace(/^\/\/\?\//, '').toLowerCase() === workspaceDir.replaceAll('\\', '/').toLowerCase())
    ?? await rpc(page, 'workspace.add', { root: workspaceDir })
  await page.getByRole('button', { name: `在 ${workspaceEntry.name} 新建任务`, exact: true }).click()
  const textarea = page.getByRole('textbox', { name: '任务说明' })
  await textarea.waitFor({ timeout: 15_000 })
  const created = (await rpc(page, 'app.bootstrap')).sessionsByWorkspace[workspaceEntry.workspaceId][0]
  await rpc(page, 'session.updateSettings', { sessionId: created.threadId, selector: modelSelector })
  await page.reload(); await textarea.waitFor()

  const statsText = () => page.evaluate(() => document.querySelector('.composer-stats')?.innerText ?? null)
  const running = () => page.evaluate(() => document.querySelector('button[aria-label="停止当前任务"], button[aria-label="正在停止"]') !== null)
  const waitIdle = async () => {
    const deadline = Date.now() + 180_000
    while (Date.now() < deadline) {
      if (!(await running())) return
      await new Promise(resolve => setTimeout(resolve, 500))
    }
    throw new Error('任务未在 180 秒内结束')
  }
  const send = async text => {
    await textarea.fill(text)
    await page.getByRole('button', { name: '发送消息', exact: true }).click()
    await page.waitForFunction(() => document.querySelector('.composer-card textarea')?.value === '')
  }
  const checkSettledStats = async () => {
    const bootstrap = await rpc(page, 'app.bootstrap')
    const session = bootstrap.sessionsByWorkspace[workspaceEntry.workspaceId][0]
    const snapshot = await rpc(page, 'session.read', { sessionId: session.threadId, limit: 40 })
    const usage = snapshot.history.summary.usage
    const hasCache = usage.usagePresent && usage.cacheUsageComplete && usage.inputTokens > 0
    const ttft = usage.ttftRequests > 0 ? `${(usage.ttftMs / usage.ttftRequests / 1000).toFixed(1)}s TTFT` : null
    await page.waitForFunction(({ present, hasCache, ttft }) => {
      const text = document.querySelector('.composer-stats')?.textContent
      return Boolean(text) === present && Boolean(text?.includes('缓存命中率')) === hasCache && (ttft === null ? !text?.includes('TTFT') : text?.includes(ttft))
    }, { present: usage.usagePresent || ttft !== null, hasCache, ttft }, { timeout: 15_000 })
    const timings = snapshot.history.turns.flatMap(turn => turn.items).flatMap(item => item.type === 'request' && item.observation.ttftMs !== undefined ? [item.observation.ttftMs] : [])
    assert.equal(usage.ttftRequests, timings.length)
    assert.equal(usage.ttftMs, timings.reduce((sum, ms) => sum + ms, 0))
    assert.ok(await page.locator('.composer-stat').evaluateAll(items => items.every(item => getComputedStyle(item).backgroundColor === 'rgba(0, 0, 0, 0)')), '统计读数不带灰底')
    return { text: await statsText(), usage, hasCache, ttft }
  }

  // 回合 1：正常完成，以实测用量建立显示基线。
  await send('只回答一个数字：1+1 等于几？')
  await waitIdle()
  report.baseline = await checkSettledStats()

  // 回合 2：已有消费不能因新请求开始而消失，不得出现 ≥。
  await send('再只回答一个数字：2+2 等于几？')
  await page.evaluate(() => {
    window.__samples = []
    window.__timer = setInterval(() => window.__samples.push(document.querySelector('.composer-stats')?.innerText ?? null), 100)
  })
  await waitIdle()
  const samples = await page.evaluate(() => { clearInterval(window.__timer); return window.__samples })
  report.turn2 = {
    samples: samples.length,
    hidden: samples.filter(text => text === null),
    withLowerBound: samples.filter(text => text?.includes('≥')),
  }
  if (report.baseline.text !== null) assert.equal(report.turn2.hidden.length, 0, '运行中已有统计不得消失')
  assert.equal(report.turn2.withLowerBound.length, 0, '不得出现 ≥ 下界标记')
  report.turn2.settled = await checkSettledStats()
  if (report.baseline.hasCache && report.turn2.settled.hasCache) {
    assert.ok(samples.every(text => text?.includes('缓存命中率')), '完整的缓存统计在运行中应保持显示')
  }

  // 回合 3：取消可能带有用量；按实际上报字段核对取消和重读后的显示。
  await send('写一段 500 字左右的短文介绍一座你熟悉的城市，越详细越好，最后列出 20 个相关关键词。')
  await page.getByRole('button', { name: '停止当前任务', exact: true }).waitFor({ timeout: 60_000 })
  await page.waitForTimeout(800)
  await page.getByRole('button', { name: '停止当前任务', exact: true }).click()
  await waitIdle()
  report.afterAbort = await checkSettledStats()

  await page.reload()
  await page.waitForSelector('.app-shell', { timeout: 30_000 })
  report.afterReload = await checkSettledStats()
  assert.deepEqual(report.afterReload.usage, report.afterAbort.usage, '重读应保留同一份消费事实')

  await page.screenshot({ path: join(output, 'stats-bar.png') })
  let scenario
  server = createServer(async (request, response) => {
    for await (const _ of request) { /* Drain the request before serving the controlled stream. */ }
    const { protocol, delay, streamed, usage } = scenario
    response.writeHead(200, { 'Content-Type': 'text/event-stream' })
    const event = body => response.write(`data: ${JSON.stringify(body)}\n\n`)
    event(protocol === 'chat' ? { choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] } : { type: 'response.created' })
    await new Promise(resolve => setTimeout(resolve, delay))
    if (streamed) {
      event(protocol === 'chat' ? { choices: [{ index: 0, delta: { reasoning_content: 'Thinking.' }, finish_reason: null }] } : { type: 'response.reasoning_summary_text.delta', delta: 'Thinking.' })
      await new Promise(resolve => setTimeout(resolve, 120))
      event(protocol === 'chat' ? { choices: [{ index: 0, delta: { content: 'OK' }, finish_reason: null }] } : { type: 'response.output_text.delta', delta: 'OK' })
    }
    if (protocol === 'chat') {
      event({ choices: [{ index: 0, delta: streamed ? {} : { content: 'OK' }, finish_reason: 'stop' }], ...(usage ? { usage: { prompt_tokens: 100, completion_tokens: 10, total_tokens: 110 } } : {}) })
      response.end('data: [DONE]\n\n')
    } else {
      event({ type: 'response.completed', response: { status: 'completed', output: [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: 'OK' }] }], ...(usage ? { usage: { input_tokens: 100, output_tokens: 10, total_tokens: 110 } } : {}) } })
      response.end()
    }
  })
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  await rpc(page, 'model.saveProvider', { provider: { providerId: 'ttft-check', displayName: 'TTFT E2E', baseUrl: `http://127.0.0.1:${server.address().port}/v1`, apiProtocol: null,
    models: ['chat', 'responses'].map(protocol => ({ modelId: protocol, displayName: protocol, apiProtocol: protocol, automaticFields: [],
      maxContextTokens: 64000, maxOutputTokens: 4096, reasoningVariants: null, defaultVariant: null, thinkingWireFormat: null,
      chatOutputTokensField: null, requiresReasoningContentForToolCalls: null })) }, apiKey: 'local-fixture' })
  report.controlled = []
  for (const next of [
    { protocol: 'chat', delay: 180, streamed: true, usage: true },
    { protocol: 'chat', delay: 480, streamed: true, usage: false },
    { protocol: 'responses', delay: 240, streamed: true, usage: false },
    { protocol: 'responses', delay: 180, streamed: false, usage: true },
  ]) {
    scenario = next
    const current = (await rpc(page, 'app.bootstrap')).sessionsByWorkspace[workspaceEntry.workspaceId][0]
    await rpc(page, 'session.updateSettings', { sessionId: current.threadId, selector: `ttft-check/${next.protocol}` })
    await page.reload(); await textarea.waitFor()
    await send('Reply OK.'); await waitIdle()
    const checked = await checkSettledStats()
    const snapshot = await rpc(page, 'session.read', { sessionId: current.threadId, limit: 40 })
    const observation = snapshot.history.turns.at(-1).items.find(item => item.type === 'request').observation
    if (next.streamed) {
      assert.ok(observation.ttftMs >= next.delay - 10, '空 role 或协议起始帧不算首 token')
      assert.ok(observation.decodeMs >= 110, '思考增量就是首 token，无需等待正文')
    } else assert.equal(observation.ttftMs, undefined, '未观察到生成增量时计时未知')
    const { ttftMs, decodeMs, durationMs, status } = observation
    report.controlled.push({ ...next, timing: { ttftMs, decodeMs, durationMs, status }, checked })
  }
  await page.screenshot({ path: join(output, 'stats-bar-controlled.png') })
  assert.deepEqual(errors, [])
  report.status = 'passed'
  console.log('stats-bar E2E PASS', JSON.stringify(report))
} catch (error) {
  report.status = 'failed'
  report.failure = String(error)
  throw error
} finally {
  writeFileSync(join(output, 'stats-bar.json'), JSON.stringify({ ...report, errors }, null, 2))
  await app.close()
  server?.closeAllConnections(); server?.close()
}
