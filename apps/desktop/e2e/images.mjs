import { mkdirSync, writeFileSync, readFileSync, unlinkSync, renameSync } from 'node:fs'
import { join } from 'node:path'
import { createServer } from 'node:http'
import { createHash } from 'node:crypto'
import assert from 'node:assert/strict'
import { setupE2E, rpc as callRpc, modelSelector } from './support.mjs'

const { output, launch } = setupE2E('electron-images')
const workspacePath = join(output, 'workspace')
mkdirSync(workspacePath, { recursive: true })
const checks = []
const errors = []
const requests = []
let mode = 'normal'
const held = []
const send = (res, body, protocol, calls = []) => {
  res.writeHead(200, { 'Content-Type': 'text/event-stream' })
  if (protocol === 'chat') {
    const delta = calls.length ? { tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: 'read', arguments: JSON.stringify(call.args) } })) } : { content: body }
    res.end(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: calls.length ? 'tool_calls' : 'stop' }], usage: { prompt_tokens: 200, completion_tokens: 20, total_tokens: 220 } })}\n\ndata: [DONE]\n\n`)
  } else {
    const output = calls.length ? calls.map(call => ({ type: 'function_call', id: call.id, call_id: call.id, name: 'read', arguments: JSON.stringify(call.args) })) : [{ type: 'message', role: 'assistant', content: [{ type: 'output_text', text: body }] }]
    res.end(`data: ${JSON.stringify({ type: 'response.completed', response: { status: 'completed', output, usage: { input_tokens: 200, output_tokens: 20, total_tokens: 220 } } })}\n\n`)
  }
}
const server = createServer(async (req, res) => {
  try {
    let raw = ''
    for await (const part of req) raw += part
    const body = JSON.parse(raw)
    requests.push(body)
    const protocol = req.url.endsWith('/responses') ? 'responses' : 'chat'
    if (mode === 'hold') { held.push({ res, protocol }); return }
    const messages = body.messages ?? body.input
    const hasResult = messages.some(message => message.tool_call_id === 'read-a' || message.call_id === 'read-a' && message.type === 'function_call_output')
    const calls = mode === 'read' && !hasResult ? [
      { id: 'read-a', args: { path: 'fixture.png', offset: 1, limit: 1 } },
      { id: 'read-b', args: { path: 'fixture.webp' } },
      { id: 'read-c', args: { path: 'plain.txt' } },
    ] : []
    send(res, mode === 'summary' ? '## Goal\nContinue.\n## Critical Context\nImage snapshots remain on disk.' : 'MODEL_OK', protocol, calls)
  } catch (error) { errors.push(String(error)); res.writeHead(500); res.end(String(error)) }
})
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
let app = await launch()
let page
let workspace
let uploads
const record = (name, result) => { checks.push({ name, result }); console.log(name, JSON.stringify(result)) }
const rpc = (method, params) => callRpc(page, method, params)
const read = sessionId => rpc('session.read', { sessionId, limit: 40, beforeTurn: null })
const settle = async sessionId => {
  const deadline = Date.now() + 360_000
  let result
  do { result = await read(sessionId); if (result.runtime.phase === 'idle') return result; await page.waitForTimeout(100) } while (Date.now() < deadline)
  throw new Error(`Task did not settle: ${sessionId}`)
}
const select = async sessionId => {
  await page.evaluate(({ sessionId, workspaceId }) => {
    const key = 'singularity.app.view.v1'
    const view = JSON.parse(localStorage.getItem(key) ?? '{}')
    localStorage.setItem(key, JSON.stringify({ ...view, selectedSessionId: sessionId, selectedWorkspaceId: workspaceId }))
  }, { sessionId, workspaceId: workspace.workspaceId })
  await page.reload()
  await page.getByRole('textbox', { name: '任务说明' }).waitFor()
  await page.waitForFunction(() => !document.querySelector('textarea[aria-label="任务说明"]')?.disabled)
}
const create = async selector => {
  const session = await rpc('session.create', { workspaceId: workspace.workspaceId })
  const id = session.history.summary.threadId
  await rpc('session.updateSettings', { sessionId: id, selector })
  await select(id)
  return id
}
const attachments = result => result.history.turns.flatMap(turn => turn.items).flatMap(item => item.images ?? [])
const bindPage = async () => {
  page = await app.firstWindow()
  page.on('pageerror', error => errors.push(error.message))
  await page.waitForSelector('.app-shell')
}
try {
  await bindPage()
  workspace = (await rpc('app.bootstrap')).workspaces.find(item => item.root.replaceAll('\\', '/').replace(/^\/\/\?\//, '').toLowerCase() === workspacePath.replaceAll('\\', '/').toLowerCase()) ?? await rpc('workspace.add', { root: workspacePath })
  const provider = {
    providerId: 'image-check', displayName: 'Image E2E', baseUrl: `http://127.0.0.1:${server.address().port}/v1`, apiProtocol: null,
    models: ['chat', 'responses'].map(protocol => ({ modelId: protocol, displayName: protocol, apiProtocol: protocol,
      automaticFields: [], maxContextTokens: 64_000, maxOutputTokens: 4096,
      reasoningVariants: null, defaultVariant: null, thinkingWireFormat: null, chatOutputTokensField: null, requiresReasoningContentForToolCalls: null })),
  }
  await rpc('model.saveProvider', { provider, apiKey: 'local-fixture' })
  uploads = await page.evaluate(() => {
    const canvas = document.createElement('canvas'); canvas.width = 400; canvas.height = 240
    const ctx = canvas.getContext('2d')
    ctx.fillStyle = 'white'; ctx.fillRect(0, 0, 400, 240)
    ctx.fillStyle = '#e02090'; for (let i = 0; i < 3; i++) ctx.fillRect(20 + i * 70, 35, 45, 45)
    ctx.fillStyle = '#00a0c0'; for (let i = 0; i < 2; i++) { ctx.beginPath(); ctx.arc(275 + i * 70, 58, 25, 0, Math.PI * 2); ctx.fill() }
    ctx.fillStyle = 'black'; ctx.font = 'bold 50px sans-serif'; ctx.fillText('V6J9', 110, 170)
    return ['png', 'jpeg', 'webp'].map(type => ({ name: `fixture.${type}`, dataUrl: canvas.toDataURL(`image/${type}`) }))
  })
  for (const upload of uploads) writeFileSync(join(workspacePath, upload.name), Buffer.from(upload.dataUrl.split(',')[1], 'base64'))
  writeFileSync(join(workspacePath, 'plain.txt'), 'MIXED_READ_OK\n')

  const id = await create('image-check/chat')
  const textbox = page.getByRole('textbox', { name: '任务说明' })
  await page.evaluate(id => localStorage.setItem(`singularity.app.view.v1:draft:${id}`, '旧版文字草稿'), id)
  await select(id)
  assert.equal(await textbox.inputValue(), '旧版文字草稿')
  assert.equal(await page.evaluate(id => localStorage.getItem(`singularity.app.view.v1:draft:${id}`), id), null)
  record('legacy-text-draft-migration', true)
  await textbox.fill('图文草稿')
  await page.getByRole('button', { name: '展开任务工具', exact: true }).click()
  await page.getByRole('button', { name: '添加图片', exact: true }).waitFor({ state: 'visible' })
  await page.waitForFunction(() => {
    const panel = document.querySelector('.t-morph[data-open="true"]')
    const last = panel?.querySelector('.composer-tools-item:last-child')
    return last && last.getBoundingClientRect().bottom <= panel.getBoundingClientRect().bottom
  })
  await page.screenshot({ path: join(output, 'tools-menu.png') })
  const chooser = page.waitForEvent('filechooser')
  await page.getByRole('button', { name: '添加图片', exact: true }).click()
  await (await chooser).setFiles(join(workspacePath, 'fixture.png'))
  await page.waitForFunction(() => document.querySelector('.t-morph[data-open="false"]'))
  await page.waitForFunction(() => document.querySelectorAll('.composer-card .image-tile').length === 1)
  await select(id)
  assert.equal(await textbox.inputValue(), '图文草稿')
  assert.equal(await page.locator('.composer-card .image-tile').count(), 1)
  await page.getByRole('button', { name: '查看图片 fixture.png', exact: true }).click()
  await page.getByRole('dialog').waitFor(); await page.keyboard.press('Escape')
  await page.locator('.image-dialog').waitFor({ state: 'detached' })
  record('file-select-draft-reload-preview-escape', true)
  const pasteOrDrop = async (kind, upload) => page.evaluate(({ kind, upload }) => {
    const bytes = Uint8Array.from(atob(upload.dataUrl.split(',')[1]), value => value.charCodeAt(0))
    const transfer = new DataTransfer(); transfer.items.add(new File([bytes], upload.name, { type: upload.dataUrl.slice(5).split(';')[0] }))
    const target = document.querySelector('textarea[aria-label="任务说明"]')
    target.dispatchEvent(kind === 'paste' ? new ClipboardEvent('paste', { clipboardData: transfer, bubbles: true, cancelable: true }) : new DragEvent('drop', { dataTransfer: transfer, bubbles: true, cancelable: true }))
  }, { kind, upload })
  await pasteOrDrop('paste', uploads[1]); await pasteOrDrop('drop', uploads[2])
  await page.waitForFunction(() => document.querySelectorAll('.composer-card .image-tile').length === 3)
  await page.screenshot({ path: join(output, 'composer-images.png') })
  await page.getByRole('button', { name: '发送消息', exact: true }).click()
  await page.waitForFunction(() => document.querySelectorAll('.timeline-user .image-tile').length === 3)
  let result = await settle(id)
  assert.equal(result.history.summary.status, 'completed')
  assert.equal(attachments(result).length, 3)
  assert.equal(await page.locator('.timeline-user .image-tile').count(), 3)
  const visualRequest = requests.find(request => request.messages?.some(message => Array.isArray(message.content) && message.content.filter(part => part.type === 'image_url').length === 3))
  assert.ok(visualRequest)
  record('paste-drop-multiple-images-send', attachments(result))

  const snapshot = attachments(result)[0]
  const pixelPath = join(process.env.SINGULARITY_HOME, 'sessions', 'images', id, snapshot.id)
  renameSync(pixelPath, `${pixelPath}.held`)
  try {
    const failure = await page.evaluate(({ sessionId, imageId }) => window.singularity.rpc({ method: 'session.imageRead', params: { sessionId, imageId } }), { sessionId: id, imageId: snapshot.id })
    assert.equal(failure.type, 'error'); assert.equal(failure.error.code, 'internal')
    assert.ok(failure.error.message.includes(snapshot.name) && failure.error.message.includes(snapshot.id))
    assert.equal((await read(id)).history.summary.threadId, id)
    record('missing-pixel-preserves-task-and-file-context', failure.error.message)
  } finally { renameSync(`${pixelPath}.held`, pixelPath) }

  const gif = Buffer.from('R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7', 'base64')
  const bmp = Buffer.alloc(58)
  bmp.write('BM'); bmp.writeUInt32LE(58, 2); bmp.writeUInt32LE(54, 10); bmp.writeUInt32LE(40, 14)
  bmp.writeInt32LE(1, 18); bmp.writeInt32LE(1, 22); bmp.writeUInt16LE(1, 26); bmp.writeUInt16LE(24, 28)
  const formats = await create('image-check/chat')
  mode = 'read'
  writeFileSync(join(workspacePath, 'fixture.png'), gif); writeFileSync(join(workspacePath, 'fixture.webp'), bmp)
  await rpc('session.submit', { sessionId: formats, text: 'Read images by their contents, independent of extensions.' })
  const normalized = attachments(await settle(formats))
  assert.equal(normalized.length, 2)
  assert.ok(normalized.every(image => image.mimeType === 'image/png' && image.width === 1 && image.height === 1))
  assert.ok(normalized.some(image => image.name.includes('首帧')))
  record('gif-first-frame-bmp-normalization-format-sniffing', normalized)
  for (const upload of uploads) writeFileSync(join(workspacePath, upload.name), Buffer.from(upload.dataUrl.split(',')[1], 'base64'))

  mode = 'hold'
  await select(id)
  await rpc('session.submit', { sessionId: id, text: 'Hold the current request.' })
  await page.waitForFunction(() => document.querySelector('.stop-button'))
  await page.locator('.composer-card input[type=file]').setInputFiles(join(workspacePath, 'fixture.png'))
  await page.waitForFunction(() => document.querySelectorAll('.composer-card .image-tile').length === 1)
  await textbox.fill('排队图片'); await textbox.press('Enter')
  await page.waitForFunction(() => document.querySelectorAll('.queued-inputs .image-tile').length === 1)
  await rpc('session.followUp', { sessionId: id, text: '另一条排队消息' })
  await page.locator('.queue-toggle').click()
  await page.waitForFunction(() => document.querySelectorAll('button[aria-label="编辑消息"]').length === 2)
  await page.evaluate(() => {
    const buttons = document.querySelectorAll('button[aria-label="编辑消息"]')
    buttons[0].click(); buttons[1].click()
  })
  await page.waitForFunction(() => document.querySelector('textarea[aria-label="编辑排队消息"]')?.value === '另一条排队消息')
  await page.waitForFunction(() => ![...document.querySelectorAll('button[aria-label="编辑消息"]')].some(button => button.disabled))
  assert.equal(await page.getByRole('textbox', { name: '编辑排队消息' }).inputValue(), '另一条排队消息')
  await page.getByRole('button', { name: '取消编辑', exact: true }).click()
  const pending = (await read(id)).runtime.pendingControls
  await rpc('session.queueWithdraw', { sessionId: id, controlId: pending[1].controlId })
  await page.waitForFunction(() => document.querySelectorAll('button[aria-label="编辑消息"]').length === 1)
  record('queue-editor-keeps-last-selection-after-image-read', true)
  await page.getByRole('button', { name: '编辑消息', exact: true }).click()
  const edit = page.getByRole('textbox', { name: '编辑排队消息' }); await edit.waitFor()
  await edit.fill('编辑后的图片消息')
  await page.getByRole('button', { name: '保存消息', exact: true }).click()
  await page.waitForFunction(() => !document.querySelector('textarea[aria-label="编辑排队消息"]'))
  await select(id)
  assert.equal(await page.locator('.queued-inputs .image-tile').count(), 1)
  await page.getByRole('button', { name: '停止当前任务', exact: true }).click()
  mode = 'normal'; for (const { res, protocol } of held.splice(0)) if (!res.destroyed) send(res, 'MODEL_OK', protocol)
  result = await settle(id)
  assert.equal(result.runtime.pendingControls.length, 1)
  assert.equal(result.runtime.pendingControls[0].text, '编辑后的图片消息')
  await page.getByRole('button', { name: '立即发送排队消息', exact: true }).click()
  result = await settle(id)
  assert.equal(result.runtime.pendingControls.length, 0)
  assert.equal(attachments(result).length, 4)
  record('queue-edit-reload-cancel-send-now', true)

  for (const protocol of ['chat', 'responses']) {
    const local = await create(`image-check/${protocol}`)
    mode = 'read'; const start = requests.length
    await rpc('session.submit', { sessionId: local, text: 'Read the two image files and text together.' })
    const readResult = await settle(local)
    assert.equal(readResult.history.summary.status, 'completed')
    assert.equal(attachments(readResult).length, 2)
    const request = requests.slice(start).at(-1)
    if (protocol === 'chat') {
      const messages = request.messages
      const toolPositions = messages.flatMap((message, index) => message.role === 'tool' ? [index] : [])
      assert.equal(toolPositions.length, 3)
      assert.ok(toolPositions.every(index => typeof messages[index].content === 'string'))
      assert.equal(messages[toolPositions.at(-1) + 1].content.filter(part => part.type === 'image_url').length, 2)
    } else {
      const results = request.input.filter(message => message.type === 'function_call_output')
      assert.equal(results.length, 3)
      assert.equal(results.filter(result => Array.isArray(result.output) && result.output.some(part => part.type === 'input_image')).length, 2)
      mode = 'normal'
      await rpc('session.submit', { sessionId: local, text: '', images: [uploads[0]] })
      assert.equal((await settle(local)).history.summary.status, 'completed')
      assert.ok(requests.at(-1).input.some(message => message.role === 'user' && Array.isArray(message.content) && message.content.some(part => part.type === 'input_image')))
    }
    mode = 'normal'
    const saved = attachments(readResult).find(image => image.name === 'fixture.png')
    assert.equal(await rpc('session.imageRead', { sessionId: local, imageId: saved.id }), uploads[0].dataUrl)
    unlinkSync(join(workspacePath, 'fixture.png')); unlinkSync(join(workspacePath, 'fixture.webp'))
    await app.close(); app = await launch(); await bindPage(); await select(local)
    const restarted = requests.length
    await rpc('session.submit', { sessionId: local, text: 'Continue using the saved images.' }); await settle(local)
    const replay = requests[restarted]
    const pixels = (replay.messages ?? replay.input)
      .flatMap(message => Array.isArray(message.content) ? message.content : Array.isArray(message.output) ? message.output : [])
      .flatMap(part => part.type === 'image_url' ? [part.image_url.url] : part.type === 'input_image' ? [part.image_url] : [])
    assert.deepEqual(new Set(pixels), new Set([uploads[0].dataUrl, uploads[2].dataUrl]))
    for (const upload of [uploads[0], uploads[2]]) writeFileSync(join(workspacePath, upload.name), Buffer.from(upload.dataUrl.split(',')[1], 'base64'))
    record(`${protocol}-restart-replays-exact-pixels-without-source`, true)
    await rpc('session.submit', { sessionId: local, text: 'x'.repeat(30000) }); await settle(local)
    mode = 'summary'; const before = requests.length
    await rpc('session.compact', { sessionId: local }); await settle(local)
    assert.ok(requests.slice(before).some(request => JSON.stringify(request).includes('data:image/')))
    const file = join(process.env.SINGULARITY_HOME, 'sessions', `${local}.jsonl`)
    const ledger = readFileSync(file, 'utf8')
    assert.ok(ledger.includes('"type":"compaction"')); assert.ok(!ledger.includes('data:image/'))
    record(`${protocol}-mixed-read-encoding-compaction-snapshot`, { sessionId: local, imageIds: attachments(readResult).map(image => image.id) })
  }
  mode = 'normal'
  const bad = await page.evaluate(({ sessionId }) => window.singularity.rpc({ method: 'session.submit', params: { sessionId, text: '', images: [{ name: 'broken.png', dataUrl: 'data:image/png;base64,aGVsbG8=' }] } }), { sessionId: id })
  assert.equal(bad.type, 'error'); record('invalid-image-is-explicit-error', bad.error.message)

  if (process.env.SINGULARITY_E2E_MODEL === '1') {
    const real = await create(modelSelector)
    await rpc('session.submit', { sessionId: real, text: '请直接看图片，回答四位字符，以及正方形和圆形各有几个。不要调用工具。', images: [uploads[0]] })
    let realResult = await settle(real)
    const answer = realResult.history.turns.flatMap(turn => turn.items).filter(item => item.type === 'message' && item.role === 'assistant').map(item => item.text).join('\n')
    assert.equal(realResult.history.summary.status, 'completed', JSON.stringify(realResult.runtime.terminal))
    assert.match(answer, /V6J9/i)
    writeFileSync(join(output, 'real-upload.json'), JSON.stringify(realResult, null, 2))
    record('real-model-upload-pixel-recognition', answer)
    const local = await create(modelSelector)
    await rpc('session.submit', { sessionId: local, text: '实际使用 read 工具读取 fixture.png，然后根据图片回答四位字符和图形数量。不要使用 bash、OCR 或其他工具。' })
    realResult = await settle(local)
    assert.equal(realResult.history.summary.status, 'completed', JSON.stringify(realResult.runtime.terminal))
    assert.ok(attachments(realResult).length > 0)
    const localAnswer = realResult.history.turns.flatMap(turn => turn.items).filter(item => item.type === 'message' && item.role === 'assistant').map(item => item.text).join('\n')
    assert.match(localAnswer, /V6J9/i)
    writeFileSync(join(output, 'real-read.json'), JSON.stringify(realResult, null, 2))
    record('real-model-local-read-pixel-recognition', localAnswer)
    unlinkSync(join(workspacePath, 'fixture.png'))
    await app.close(); app = await launch(); await bindPage(); await select(local)
    const recovered = await read(local)
    assert.equal(await rpc('session.imageRead', { sessionId: local, imageId: attachments(recovered)[0].id }), uploads[0].dataUrl)
    await rpc('session.submit', { sessionId: local, text: '原图文件已经删除。只根据对话里的图片再回答那四位字符，不调用工具。' })
    const continued = await settle(local)
    assert.equal(continued.history.summary.status, 'completed')
    assert.match(continued.history.turns.at(-1).items.filter(item => item.type === 'message' && item.role === 'assistant').map(item => item.text).join('\n'), /V6J9/i)
    await page.screenshot({ path: join(output, 'recovered-images.png') })
    record('real-model-restart-deleted-source-follow-up', true)
  }
  assert.deepEqual(errors, [])
} finally {
  if (page && !page.isClosed()) { await page.screenshot({ path: join(output, 'last-screen.png') }); writeFileSync(join(output, 'last-ui.txt'), await page.locator('body').innerText()) }
  writeFileSync(join(output, 'images.json'), JSON.stringify({ command: 'node apps/desktop/e2e/images.mjs', home: process.env.SINGULARITY_HOME, executable: process.env.SINGULARITY_E2E_PACKAGED,
    executableSha256: process.env.SINGULARITY_E2E_PACKAGED ? createHash('sha256').update(readFileSync(process.env.SINGULARITY_E2E_PACKAGED)).digest('hex') : null,
    checks, errors, model: process.env.SINGULARITY_E2E_MODEL === '1' ? modelSelector : null,
    requests: requests.map(request => JSON.parse(JSON.stringify(request, (key, value) => typeof value === 'string' && value.startsWith('data:image/') ? `[image data: ${value.length} chars]` : value))) }, null, 2))
  await app.close(); server.closeAllConnections(); server.close()
}
