import { join, resolve } from 'node:path'
import { mkdirSync, writeFileSync } from 'node:fs'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import assert from 'node:assert/strict'
import { setupE2E, rpc } from './support.mjs'

// 统计条行为回归：只有上报 usage 的请求参与合计——运行中的请求不打断命中率显示、
// 不出现 ≥ 下界标记，中途取消的请求不毒化会话命中率（重读冻结聚合后仍显示）。
const { desktop, output, launch } = setupE2E('e2e-stats-bar')
const app = await launch()
const errors = []
const report = {}
try {
  const page = await app.firstWindow()
  page.on('pageerror', error => errors.push(error.message))
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()) })
  await page.waitForSelector('.app-shell', { timeout: 30_000 })

  // 真实 UI 路径：原生对话框添加项目 → 新建任务 → 输入发送；RPC 只用于读取状态。
  const workspaceDir = resolve(join(output, 'workspace'))
  mkdirSync(workspaceDir, { recursive: true })
  const nativeDialog = async () => {
    const action = promisify(execFile)('pwsh.exe', ['-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', join(desktop, 'e2e/folder-dialog.ps1'), '-OwnerProcessId', String(await app.evaluate(() => process.pid)), '-Folder', workspaceDir], { timeout: 25_000 })
    await page.getByRole('button', { name: '添加项目', exact: true }).click()
    await action
  }
  await nativeDialog()
  const workspaceEntry = (await rpc(page, 'app.bootstrap')).workspaces.find(item => item.root.replaceAll('\\', '/').endsWith('/workspace'))
  assert.ok(workspaceEntry, 'workspace 应注册成功')
  await page.getByRole('button', { name: `在 ${workspaceEntry.name} 新建任务`, exact: true }).click()
  const textarea = page.getByRole('textbox', { name: '任务说明' })
  await textarea.waitFor({ timeout: 15_000 })

  const statsText = () => page.evaluate(() => document.querySelector('.composer-stats')?.innerText ?? null)
  const running = () => page.evaluate(() => document.querySelector('button[aria-label="停止当前任务"], button[aria-label="正在停止"]') !== null)
  const waitIdle = async () => {
    for (;;) {
      if (!(await running())) return
      await new Promise(resolve => setTimeout(resolve, 500))
    }
  }
  const send = async text => { await textarea.fill(text); await page.getByRole('button', { name: '发送消息', exact: true }).click() }
  const waitHitVisible = () => page.waitForFunction(
    () => document.querySelector('.composer-stats')?.textContent.includes('缓存命中率'),
    null, { timeout: 180_000 })

  // 回合 1：正常完成，命中率成为统计条基线。
  await send('只回答一个数字：1+1 等于几？')
  await waitHitVisible()
  report.baseline = await statsText()
  assert.ok(report.baseline?.includes('缓存命中率'), `回合完成后统计条应显示命中率：${report.baseline}`)
  await waitIdle()

  // 回合 2：请求运行期间持续采样——统计条必须全程显示命中率，不得出现 ≥。
  await send('再只回答一个数字：2+2 等于几？')
  await page.evaluate(() => {
    window.__samples = []
    window.__timer = setInterval(() => window.__samples.push(document.querySelector('.composer-stats')?.innerText ?? null), 100)
  })
  await waitIdle()
  const samples = await page.evaluate(() => { clearInterval(window.__timer); return window.__samples })
  report.turn2 = {
    samples: samples.length,
    hidden: samples.filter(text => text === null || !text.includes('缓存命中率')),
    withLowerBound: samples.filter(text => text?.includes('≥')),
  }
  assert.ok(report.turn2.samples > 5, `采样数量不足：${report.turn2.samples}`)
  assert.equal(report.turn2.hidden.length, 0, `运行中统计条或命中率不得消失：${JSON.stringify(report.turn2.hidden)}`)
  assert.equal(report.turn2.withLowerBound.length, 0, '不得出现 ≥ 下界标记')

  // 回合 3：中途取消——取消的请求没有用量，命中率必须保持显示；重读历史后同样显示。
  await send('写一段 500 字左右的短文介绍一座你熟悉的城市，越详细越好，最后列出 20 个相关关键词。')
  await page.getByRole('button', { name: '停止当前任务', exact: true }).waitFor({ timeout: 60_000 })
  await page.waitForTimeout(800)
  await page.getByRole('button', { name: '停止当前任务', exact: true }).click()
  await waitIdle()
  report.afterAbort = await statsText()
  assert.ok(report.afterAbort?.includes('缓存命中率'), `取消后命中率不得消失：${report.afterAbort}`)

  await page.reload()
  await page.waitForSelector('.app-shell', { timeout: 30_000 })
  await page.waitForSelector('.composer-stats', { timeout: 60_000 })
  report.afterReload = await statsText()
  assert.ok(report.afterReload?.includes('缓存命中率'), `含取消记录的会话重读后命中率不得消失：${report.afterReload}`)

  await page.screenshot({ path: join(output, 'stats-bar.png') })
  writeFileSync(join(output, 'stats-bar.json'), JSON.stringify({ ...report, errors }, null, 2))
  assert.deepEqual(errors, [])
  console.log('stats-bar E2E PASS', JSON.stringify(report))
} finally {
  await app.close()
}
