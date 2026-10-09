import { mkdirSync, writeFileSync, readFileSync, unlinkSync } from 'node:fs'
import { join } from 'node:path'
import { createHash } from 'node:crypto'
import assert from 'node:assert/strict'
import { setupE2E, rpc as callRpc, modelSelector } from './support.mjs'

const { output, launch } = setupE2E('electron-images')
const workspacePath = join(output, 'workspace')
mkdirSync(workspacePath, { recursive: true })
const checks = []
const errors = []
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
  await page.locator('.session-main').filter({ has: page.getByText(sessionId, { exact: true }) }).click()
  await page.reload()
  await page.getByRole('textbox', { name: '任务说明' }).waitFor()
  await page.waitForFunction(() => !document.querySelector('textarea[aria-label="任务说明"]')?.disabled)
}
const create = async () => {
  const session = await rpc('session.create', { workspaceId: workspace.workspaceId })
  const id = session.history.summary.threadId
  await rpc('session.rename', { sessionId: id, name: id })
  await rpc('session.updateSettings', { sessionId: id, selector: modelSelector })
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

  const id = await create()
  const textbox = page.getByRole('textbox', { name: '任务说明' })
  await select(id)
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
  const bad = await page.evaluate(({ sessionId }) => window.singularity.rpc({ method: 'session.submit', params: { sessionId, text: '', images: [{ name: 'broken.png', dataUrl: 'data:image/png;base64,aGVsbG8=' }] } }), { sessionId: id })
  assert.equal(bad.type, 'error'); record('invalid-image-is-explicit-error', bad.error.message)

  if (process.env.SINGULARITY_E2E_MODEL === '1') {
    const real = await create()
    await rpc('session.submit', { sessionId: real, text: '请直接看图片，回答四位字符，以及正方形和圆形各有几个。不要调用工具。', images: [uploads[0]] })
    let realResult = await settle(real)
    const answer = realResult.history.turns.flatMap(turn => turn.items).filter(item => item.type === 'message' && item.role === 'assistant').map(item => item.text).join('\n')
    assert.equal(realResult.history.summary.status, 'completed', JSON.stringify(realResult.runtime.terminal))
    assert.match(answer, /V6J9/i)
    writeFileSync(join(output, 'real-upload.json'), JSON.stringify(realResult, null, 2))
    record('real-model-upload-pixel-recognition', answer)
    const local = await create()
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
    checks, errors, model: process.env.SINGULARITY_E2E_MODEL === '1' ? modelSelector : null }, null, 2))
  await app.close()
}
