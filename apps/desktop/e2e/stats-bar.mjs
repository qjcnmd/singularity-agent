import { join, resolve } from 'node:path'
import { mkdirSync, writeFileSync } from 'node:fs'
import { execFile } from 'node:child_process'
import { promisify } from 'node:util'
import assert from 'node:assert/strict'
import { setupE2E, rpc } from './support.mjs'

// 统计条行为回归：按提供方实际用量决定缓存命中率是否可显示，
// 运行和取消不把未上报的缓存明细伪装为零命中。
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
    const snapshot = await rpc(page, 'session.read', { workspaceId: workspaceEntry.workspaceId, sessionId: session.threadId, limit: 40 })
    const usage = snapshot.history.summary.usage
    const hasCache = usage.usagePresent && usage.cacheUsageComplete && usage.inputTokens > 0
    await page.waitForFunction(({ present, hasCache }) => {
      const text = document.querySelector('.composer-stats')?.textContent
      return Boolean(text) === present && Boolean(text?.includes('缓存命中率')) === hasCache
    }, { present: usage.usagePresent, hasCache }, { timeout: 15_000 })
    return { text: await statsText(), usage, hasCache }
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
  if (report.baseline.usage.usagePresent) assert.equal(report.turn2.hidden.length, 0, '运行中已有消费统计不得消失')
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
}
