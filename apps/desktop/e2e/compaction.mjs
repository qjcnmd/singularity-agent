import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import assert from 'node:assert/strict'
import { setupE2E, rpc, modelSelector, ensureWorkspace } from './support.mjs'

// 通过真实提供方检查完整压缩操作。失败使用同一提供方拒绝的模型 ID，不模拟响应。
const { output, launch } = setupE2E('e2e-compaction')
const app = await launch()
const providerId = `compaction-check-${Date.now()}`
const report = {}
let installed = false
let page
try {
  page = await app.firstWindow()
  const errors = []
  page.on('pageerror', error => errors.push(error.message))
  await page.waitForSelector('.app-shell')
  const bootstrap = await rpc(page, 'app.bootstrap')
  const slash = modelSelector.indexOf('/')
  const sourceId = modelSelector.slice(0, slash)
  const sourceModelId = modelSelector.slice(slash + 1).split('#')[0]
  const source = bootstrap.modelCatalog.providers.find(provider => provider.providerId === sourceId)
  const sourceModel = source?.models.find(model => model.modelId === sourceModelId)
  assert.ok(sourceModel, '配置中存在真实测试模型')
  const auth = JSON.parse(readFileSync(join(process.env.SINGULARITY_HOME, 'auth.json'), 'utf8'))
  const apiKey = auth.providers[sourceId].api_key
  const model = { ...sourceModel, maxOutputTokens: 2048,
    defaultVariant: sourceModel.reasoningVariants?.some(variant => variant.id === 'off') ? 'off' : sourceModel.defaultVariant }
  const unavailable = { ...model, modelId: 'singularity-compaction-unavailable-model', maxContextTokens: 4096, maxOutputTokens: 1024 }
  const provider = { providerId, displayName: '压缩回归检查', baseUrl: source.baseUrl,
    apiProtocol: source.apiProtocol, models: [model, unavailable] }
  await rpc(page, 'model.saveProvider', { provider, apiKey })
  installed = true
  const selector = `${providerId}/${model.modelId}`
  const workspace = await ensureWorkspace(page, join(output, 'workspace'))
  const created = await rpc(page, 'session.create', { workspaceId: workspace.workspaceId })
  const sessionId = created.history.summary.threadId
  const scope = { sessionId }
  await rpc(page, 'session.updateSettings', { ...scope, selector })
  const title = `压缩验证 ${Date.now()}`
  await rpc(page, 'session.rename', { ...scope, name: title })
  await page.locator('.session-main').filter({ hasText: title }).click()
  await page.getByRole('textbox', { name: '任务说明' }).waitFor()

  const read = () => rpc(page, 'session.read', { ...scope, beforeTurn: null, limit: 40 })
  const idle = async () => {
    const deadline = Date.now() + 180_000
    while (Date.now() < deadline) {
      const snapshot = await read()
      if (snapshot.runtime.phase === 'idle') return snapshot
      await page.waitForTimeout(100)
    }
    throw new Error('压缩检查超过 180 秒')
  }
  const compactions = snapshot => snapshot.history.turns.flatMap(turn => turn.items)
    .filter(item => item.type === 'compaction' || item.type === 'compaction_result')
  const observe = () => page.evaluate(sessionId => {
    window.__compactionCheck?.unsubscribe()
    const state = { frames: [], probes: [], unsubscribe: null }
    state.unsubscribe = window.singularity.onFrame(frame => {
      if (frame.type !== 'turn_event' || frame.sessionId !== sessionId) return
      state.frames.push(frame.payload)
      if (frame.payload.method === 'compaction/updated' && frame.payload.params.content.status === 'running') {
        state.probes.push(window.singularity.rpc({ method: 'session.abort', params: { sessionId } }))
        state.probes.push(window.singularity.rpc({ method: 'session.compact', params: { sessionId } }))
      }
    })
    window.__compactionCheck = state
  }, sessionId)
  const observed = () => page.evaluate(async () => ({
    frames: window.__compactionCheck.frames,
    probes: await Promise.all(window.__compactionCheck.probes),
  }))
  const checkOperation = async (expected, turnId = null) => {
    const snapshot = await idle()
    const events = await observed()
    const updates = events.frames.filter(frame => frame.method === 'compaction/updated')
    assert.equal(updates.length, 2, '一次压缩只有开始和结束两次更新')
    assert.equal(updates[0].params.content.status, 'running')
    assert.equal(updates[1].params.content.type === 'compaction' ? 'completed' : updates[1].params.content.status, expected)
    assert.equal(updates[0].params.content.id, updates[1].params.content.id, '开始和结束共用身份')
    if (turnId === null) assert.equal(updates[0].params.turnId, null)
    else assert.notEqual(updates[0].params.turnId, null)
    const saved = compactions(snapshot).find(item => item.id === updates[1].params.content.id)
    assert.deepEqual(saved, updates[1].params.content, '实时结果和重读历史一致')
    for (const probe of events.probes) assert.equal(probe.type, 'error', '压缩期间拒绝停止与重复执行')
    await page.waitForFunction(count => document.querySelectorAll('.timeline-compaction').length === count, compactions(snapshot).length)
    assert.equal(await page.locator('.turn-error').count(), 0, '压缩失败不重复显示为回合错误')
    return { snapshot, events, saved }
  }

  // 空会话无需调用模型，也应留下真实结果。
  await rpc(page, 'session.compact', scope)
  const empty = await idle()
  assert.equal(compactions(empty).at(-1).status, 'completed')
  assert.equal(empty.history.summary.turnCount, 0)
  report.empty = compactions(empty).at(-1)

  // 两段普通长消息形成可压缩前缀；不是伪造模型消息或直接改写会话文件。
  const material = Array.from({ length: 420 }, (_, index) =>
    `条目 ${index}: alpha beta gamma delta epsilon zeta eta theta iota kappa lambda.\n`).join('')
  for (let turn = 0; turn < 2; turn++) {
    await rpc(page, 'session.submit', { ...scope, text: `这是第 ${turn + 1} 批测试材料。只回复“已记录”，不要复述材料。\n${material}` })
    const snapshot = await idle()
    assert.equal(snapshot.history.summary.status, 'completed')
  }
  const beforeManual = (await read()).history.summary.turnCount
  await rpc(page, 'session.updateSettings', { ...scope, selector: `${providerId}/${unavailable.modelId}` })
  await observe()
  await rpc(page, 'session.compact', scope)
  const manualFailure = await checkOperation('failed')
  assert.equal(manualFailure.snapshot.history.summary.turnCount, beforeManual, '手动压缩不新增对话回合')
  assert.equal(manualFailure.snapshot.runtime.terminal, null, '已保存失败不再附加运行时错误')
  report.manualFailure = manualFailure.saved
  await page.screenshot({ path: join(output, 'manual-failure.png') })

  await observe()
  await rpc(page, 'session.submit', { ...scope, text: '继续检查已有材料，只回复“已记录”。' })
  const autoFailure = await checkOperation('failed', 'active')
  assert.equal(autoFailure.snapshot.history.summary.status, 'failed')
  assert.equal(autoFailure.snapshot.history.turns.at(-1).error.itemId, autoFailure.saved.id)
  report.autoFailure = autoFailure.saved

  await rpc(page, 'session.updateSettings', { ...scope, selector })
  await observe()
  await rpc(page, 'session.compact', scope)
  const manual = await checkOperation('completed')
  assert.equal(manual.saved.type, 'compaction')
  assert.ok(!manual.events.frames.some(frame => frame.method === 'provider/attempt' && frame.params.observation.purpose === 'generation'),
    '手动压缩结束后等待输入')
  report.manualSuccess = { id: manual.saved.id, summaryLength: manual.saved.summary.length }

  // 选择介于现有上下文与加入下一段材料后的容量，让真实自动入口触发。
  const ledger = readFileSync(join(process.env.SINGULARITY_HOME, 'sessions', `${sessionId}.jsonl`), 'utf8')
    .trim().split('\n').map(line => JSON.parse(line))
  const checkpoint = ledger.findLast(entry => entry.type === 'compaction').compaction
  const definitions = checkpoint.definitions
  const tokens = text => Math.ceil(text.length / 4)
  const overhead = definitions.messages.reduce((total, message) => total + tokens(message.content) + 4, 0)
    + tokens(JSON.stringify(definitions.tools)) + 4
  const retained = ledger.slice(ledger.findIndex(entry => entry.id === checkpoint.firstKeptEntryId))
    .filter(entry => entry.type === 'message')
    .reduce((total, entry) => total + 4 + entry.message.content.reduce((n, part) => n + (part.type === 'text' ? tokens(part.text) + 4 : 0), 0), 0)
  const window = Math.ceil(overhead + retained + tokens(checkpoint.summary) + 64 + tokens(material) / 2)
  assert.ok(window > 4096)
  await rpc(page, 'model.saveProvider', { provider: { ...provider, models: [{ ...model, maxContextTokens: window }, unavailable] } })
  await observe()
  await rpc(page, 'session.submit', { ...scope, text: `新的测试材料。只回复“完成”，不要调用工具。\n${material}` })
  const auto = await checkOperation('completed', 'active')
  assert.equal(auto.saved.type, 'compaction')
  assert.equal(auto.snapshot.history.summary.status, 'completed', '自动压缩后原任务正常完成')
  const finish = auto.events.frames.findIndex(frame => frame.method === 'compaction/updated' && frame.params.content.type === 'compaction')
  assert.ok(auto.events.frames.slice(finish + 1).some(frame =>
    frame.method === 'provider/attempt' && frame.params.observation.purpose === 'generation'), '完成压缩后继续原任务')
  report.autoSuccess = { id: auto.saved.id, summaryLength: auto.saved.summary.length, window }
  const savedItems = compactions(auto.snapshot)
  await page.reload()
  await page.waitForSelector('.app-shell')
  await page.waitForFunction(count => document.querySelectorAll('.timeline-compaction').length === count, savedItems.length)
  assert.equal(await page.locator('.turn-error').count(), 0)
  assert.deepEqual(compactions(await read()), savedItems, '刷新后结果和身份保持一致')
  await page.screenshot({ path: join(output, 'compaction-recovered.png') })
  assert.deepEqual(errors, [])
  report.sessionId = sessionId
  writeFileSync(join(output, 'results.json'), JSON.stringify(report, null, 2))
  console.log(JSON.stringify(report))
} finally {
  if (installed && page && !page.isClosed()) await rpc(page, 'model.removeProvider', { providerId })
  await app.close()
}
