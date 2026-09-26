import { join } from 'node:path'
import { mkdirSync, writeFileSync } from 'node:fs'
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { setupE2E, rpc, modelSelector } from './support.mjs'

const { output, launch } = setupE2E('electron-lifecycle')
mkdirSync(join(output, 'workspace'), { recursive: true })
let app = await launch()
let directoryRequested
const requested = new Promise(resolve => { directoryRequested = resolve })
// A slow read-only provider query must not delay accepted task cancellation on exit.
const directory = createServer(() => directoryRequested())
await new Promise(resolve => directory.listen(0, '127.0.0.1', resolve))
try {
  let page = await app.firstWindow()
  await page.waitForSelector('.app-shell')
  const workspace = await rpc(page, 'workspace.add', { root: join(output, 'workspace') })
  const session = await rpc(page, 'session.create', { workspaceId: workspace.workspaceId })
  const scope = { workspaceId: workspace.workspaceId, sessionId: session.history.summary.threadId }
  await rpc(page, 'session.updateSettings', { ...scope, selector: modelSelector })
  await rpc(page, 'session.submit', { ...scope, text: '执行 bash sleep 60，然后回复完成。这是退出取消验证。' })
  await page.waitForTimeout(1500)
  const pendingQuery = rpc(page, 'model.discover', { providerId: 'slow-directory', baseUrl: `http://127.0.0.1:${directory.address().port}/v1`, apiProtocol: 'chat', apiKey: null }).catch(error => error.message)
  await requested
  const start = Date.now()
  await app.close()
  await pendingQuery
  const shutdownMs = Date.now() - start
  assert.ok(shutdownMs < 10_000, `Graceful shutdown exceeded deadline: ${shutdownMs}`)
  app = await launch()
  page = await app.firstWindow()
  await page.waitForSelector('.app-shell')
  const result = await rpc(page, 'session.read', { ...scope, limit: 40, beforeTurn: null })
  assert.equal(result.runtime.phase, 'idle')
  assert.equal(result.history.summary.status, 'interrupted')
  const invalid = await page.evaluate(() => window.singularity.rpc({ version: 0, method: 'app.bootstrap', params: {} }))
  assert.equal(invalid.ok, false)
  assert.equal(invalid.error.code, 'invalid_request')
  const evidence = { scope, shutdownMs, recoveredStatus: result.history.summary.status, invalidVersion: invalid.error.code }
  writeFileSync(join(output, 'lifecycle.json'), JSON.stringify(evidence, null, 2))
  console.log(JSON.stringify(evidence))
} finally {
  await app.close()
  directory.closeAllConnections()
  directory.close()
}
