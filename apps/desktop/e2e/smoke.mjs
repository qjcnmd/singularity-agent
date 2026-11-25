
import assert from 'node:assert/strict'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { _electron as electron } from 'playwright-core'

const require = createRequire(import.meta.url)
const electronExecutable = require('electron')
import {
  createGate,
  messagesTextEvents,
  messagesToolEvents,
  startMockModelServer,
} from './mock-model-server.mjs'

const here = dirname(fileURLToPath(import.meta.url))
const desktopDir = resolve(here, '..')

const packagedArg = process.argv.find(arg => arg.startsWith('--packaged='))
const exeArg = process.argv.find(arg => arg.startsWith('--exe='))
const packagedOnly = Boolean(packagedArg)
const customExe = packagedArg
  ? resolve(packagedArg.slice('--packaged='.length))
  : exeArg
    ? resolve(exeArg.slice('--exe='.length))
    : null

function toDisplayPath(p) {
  return resolve(p).replace(/\\/g, '/')
}

function createIsolatedEnv(baseUrl) {
  const root = mkdtempSync(join(tmpdir(), 'singularity-e2e-'))
  const home = join(root, 'home')
  const workspace = join(root, 'workspace')
  mkdirSync(home, { recursive: true })
  mkdirSync(workspace, { recursive: true })

  writeFileSync(join(workspace, 'readme.md'), '# E2E Sample\n\nWORKSPACE_FILE_BODY\n', 'utf8')
  const skillDir = join(workspace, '.singularity', 'skills', 'e2e-skill')
  mkdirSync(skillDir, { recursive: true })
  writeFileSync(
    join(skillDir, 'SKILL.md'),
    '---\nname: e2e-skill\ndescription: E2E sample skill\n---\nSkill body\n',
    'utf8',
  )

  writeFileSync(
    join(home, 'workspaces.json'),
    JSON.stringify({ workspaces: [{ root: toDisplayPath(workspace) }] }, null, 2),
    'utf8',
  )
  writeFileSync(
    join(home, 'config.json'),
    JSON.stringify(
      {
        default_model: { provider: 'e2e-local', model: 'e2e-model', reasoning: 'high' },
        providers: {
          'e2e-local': {
            api_key: 'sk-e2e-secret-keep',
            base_url: baseUrl,
            api_protocol: 'messages',
            models: {
              'e2e-model': {
                max_context_tokens: 64000,
                max_output_tokens: 4096,
                reasoning_variants: ['off', 'high'],
              },
            },
          },
        },
      },
      null,
      2,
    ),
    'utf8',
  )

  const appData = process.env.APPDATA || join(process.env.USERPROFILE || tmpdir(), 'AppData', 'Roaming')
  const userDataDir = join(
    appData,
    'Singularity',
    createHash('sha256').update(resolve(home).toLowerCase()).digest('hex').slice(0, 16),
  )

  return {
    root,
    home,
    workspace,
    userDataDir,
    cleanup() {
      for (const dir of [root, userDataDir]) {
        for (let i = 0; i < 6; i += 1) {
          try {
            rmSync(dir, { recursive: true, force: true })
            break
          } catch {
            Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100)
          }
        }
      }
    },
  }
}

async function launchApp(home) {
  const env = { ...process.env, SINGULARITY_HOME: home, SINGULARITY_LOG: 'warn' }
  delete env.ELECTRON_RUN_AS_NODE
  const app = customExe
    ? await electron.launch({ executablePath: customExe, env })
    : await electron.launch({
        executablePath: electronExecutable,
        args: [desktopDir],
        cwd: desktopDir,
        env,
      })
  const page = await app.firstWindow()
  await page.waitForLoadState('domcontentloaded')
  await page.getByRole('button', { name: /在 workspace 新建任务/ }).waitFor({ state: 'visible', timeout: 20_000 })
  await page.getByRole('button', { name: /在 workspace 新建任务/ }).click()
  await page.getByLabel('任务说明').waitFor({ state: 'visible', timeout: 20_000 })
  return { app, page }
}

async function waitForIdle(page) {
  await page.getByRole('button', { name: '发送消息' }).waitFor({ state: 'visible', timeout: 20_000 })
}

async function submitPrompt(page, text) {
  const input = page.getByLabel('任务说明')
  await input.fill(text)
  await input.press('Enter')
}

async function runPackagedSmoke(page, mock) {
  console.log('  [packaged] cold start -> stream reply -> reload recovery')
  mock.enqueue(
    messagesTextEvents({
      thinking: 'packaged thought',
      text: 'PACKAGED_SMOKE_REPLY_OK',
    }),
  )
  await submitPrompt(page, 'packaged smoke prompt')
  await page.getByText('PACKAGED_SMOKE_REPLY_OK').waitFor({ state: 'visible', timeout: 20_000 })
  await page.reload()
  await page.getByText('PACKAGED_SMOKE_REPLY_OK').waitFor({ state: 'visible', timeout: 20_000 })
}

async function runFullSmoke(page, mock) {
  console.log('  [1/4] cold start, rich rendering (KaTeX + Mermaid + tool card) & reload recovery')
  await page.emulateMedia({ reducedMotion: 'reduce' })
  mock.enqueue(
    messagesToolEvents({
      calls: [{ id: 'read_1', name: 'read', args: { path: 'readme.md' } }],
    }),
  )
  const richMarkdown = [
    'RICH_RENDER_MARKER',
    '',
    '行内公式：$E = mc^2$',
    '',
    '```mermaid',
    'flowchart LR',
    '  A[Start] --> B[Verified]',
    '```',
    '',
    '查看文件：`readme.md`',
  ].join('\n')
  mock.enqueue(
    messagesTextEvents({
      thinking: 'checking readme before answering',
      text: richMarkdown,
    }),
  )

  await submitPrompt(page, 'inspect readme and render diagram')
  await page.getByText('RICH_RENDER_MARKER').waitFor({ state: 'visible', timeout: 20_000 })
  await page.locator('.katex').first().waitFor({ state: 'visible', timeout: 15_000 })
  await page.locator('.mermaid-diagram svg').first().waitFor({ state: 'visible', timeout: 15_000 })
  const mermaidBox = await page.locator('.mermaid-diagram svg').first().boundingBox()
  assert.ok(mermaidBox && mermaidBox.width > 40 && mermaidBox.height > 20, 'Mermaid SVG must have non-zero size under reducedMotion')

  await page.getByRole('link', { name: 'readme.md' }).first().click()
  const filePanel = page.getByRole('complementary', { name: '文件' })
  await filePanel.waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.getByText('WORKSPACE_FILE_BODY').waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.getByRole('button', { name: '关闭 readme.md' }).click()
  await filePanel.waitFor({ state: 'hidden', timeout: 10_000 })

  await page.reload()
  await page.getByText('RICH_RENDER_MARKER').waitFor({ state: 'visible', timeout: 20_000 })

  console.log('  [2/4] multi-session isolation, stop, question panel, error recovery & session delete')
  const gateABeforeDelta = createGate()
  const gateAHoldAfterDelta = createGate()
  const deltaTransmitted = createGate()
  mock.enqueue(
    messagesTextEvents({ text: 'SESSION_A_STREAM_SHOULD_NOT_LEAK' }),
    {
      matchText: 'long task on session A',
      gates: [
        { afterIndex: 1, gate: gateABeforeDelta },
        { afterIndex: 2, gate: gateAHoldAfterDelta, onReached: deltaTransmitted.release },
      ],
    },
  )
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  await submitPrompt(page, 'long task on session A')
  await page.getByRole('button', { name: '停止当前回合' }).waitFor({ state: 'visible', timeout: 15_000 })

  mock.enqueue(
    messagesTextEvents({ text: 'SESSION_B_CLEAN_REPLY' }),
    { matchText: 'quick task on session B' },
  )
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  await submitPrompt(page, 'quick task on session B')
  await page.getByText('SESSION_B_CLEAN_REPLY').waitFor({ state: 'visible', timeout: 15_000 })
  gateABeforeDelta.release()
  await deltaTransmitted.promise
  await page.waitForTimeout(150)
  assert.equal(await page.getByText('SESSION_A_STREAM_SHOULD_NOT_LEAK').count(), 0, 'session A text must not leak into session B')

  await page.getByRole('button', { name: /正在运行/ }).first().click()
  await page.getByRole('button', { name: '停止当前回合' }).waitFor({ state: 'visible', timeout: 10_000 })
  await page.getByRole('button', { name: '停止当前回合' }).click()
  gateAHoldAfterDelta.release()
  await waitForIdle(page)

  mock.enqueue(
    messagesToolEvents({
      calls: [
        {
          id: 'ask_e2e',
          name: 'ask_user_question',
          args: {
            questions: [
              {
                id: 'choice',
                question: 'Choose E2E path?',
                options: [{ label: 'Option Alpha' }, { label: 'Option Beta' }],
              },
            ],
          },
        },
      ],
    }),
  )
  mock.enqueue(messagesTextEvents({ text: 'QUESTION_ANSWER_CONFIRMED' }))
  await submitPrompt(page, 'ask me a question')
  const questionForm = page.getByRole('form', { name: '回答问题' })
  await questionForm.waitFor({ state: 'visible', timeout: 15_000 })
  await questionForm.getByRole('radio', { name: /Option Alpha/ }).click()
  await questionForm.getByRole('button', { name: /提交|发送|确认/ }).click()
  await page.getByText('QUESTION_ANSWER_CONFIRMED').waitFor({ state: 'visible', timeout: 15_000 })

  mock.enqueueError(400, {
    error: { code: 'insufficient_quota', message: 'E2E_SIMULATED_QUOTA_ERR' },
  })
  mock.enqueue(messagesTextEvents({ text: 'RECOVERED_AFTER_UI_ERROR' }))
  await submitPrompt(page, 'trigger upstream api error')
  await page.getByText(/E2E_SIMULATED_QUOTA_ERR/).first().waitFor({ state: 'visible', timeout: 15_000 })
  await waitForIdle(page)
  await submitPrompt(page, 'recover after api error')
  await page.getByText('RECOVERED_AFTER_UI_ERROR').waitFor({ state: 'visible', timeout: 15_000 })

  const sessionRows = page.locator('.session-row')
  const countBeforeDelete = await sessionRows.count()
  assert.ok(countBeforeDelete >= 2, 'expected at least 2 sessions before UI delete')
  await sessionRows.first().click({ button: 'right' })
  await page.getByRole('menuitem', { name: '删除任务' }).click()
  const deleteDialog = page.getByRole('dialog', { name: '删除任务' })
  await deleteDialog.waitFor({ state: 'visible', timeout: 10_000 })
  await deleteDialog.getByRole('button', { name: '删除', exact: true }).click()
  await deleteDialog.waitFor({ state: 'hidden', timeout: 10_000 })
  await page.waitForFunction(
    expected => document.querySelectorAll('.session-row').length === expected,
    countBeforeDelete - 1,
    { timeout: 10_000 },
  )

  console.log('  [3/4] draft persistence & theme persistence across reload')
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  const composer = page.getByLabel('任务说明')
  await composer.fill('UNSENT_DRAFT_SURVIVES_RELOAD')
  await page.waitForTimeout(450)
  await page.locator('.session-main').first().click()
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  assert.equal(await composer.inputValue(), 'UNSENT_DRAFT_SURVIVES_RELOAD')

  await page.getByRole('button', { name: '设置' }).click()
  await page.getByRole('radio', { name: '深色' }).click()
  await page.getByRole('button', { name: '关闭设置' }).click()
  await page.reload()
  await page.getByLabel('任务说明').waitFor({ state: 'visible', timeout: 20_000 })
  assert.equal(await page.getByLabel('任务说明').inputValue(), 'UNSENT_DRAFT_SURVIVES_RELOAD')
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'dark')

  console.log('  [4/4] settings provider / web search / instructions / skills / mcp')
  const globalInstructionsCapsule = page.getByRole('switch', { name: /全局指令/ })
  await globalInstructionsCapsule.waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal(await globalInstructionsCapsule.locator('.capsule-tokens').count(), 0, 'global instructions should start with 0 tokens')
  await page.getByRole('button', { name: '设置' }).click()
  const settingsDialog = page.getByRole('dialog', { name: '设置' })
  await settingsDialog.getByRole('button', { name: '模型', exact: true }).click()
  await settingsDialog.getByRole('button', { name: /e2e-local/ }).click()
  await settingsDialog.getByLabel('显示名称').fill('E2E Local Updated')
  await settingsDialog.getByRole('button', { name: '保存配置' }).click()
  await settingsDialog.getByText('E2E Local Updated').waitFor({ state: 'visible', timeout: 10_000 })
  await settingsDialog.getByText('已配置密钥').waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal(await page.getByText('sk-e2e-secret-keep').count(), 0, 'API key must stay redacted in UI')

  await settingsDialog.getByRole('button', { name: '联网搜索', exact: true }).click()
  const webKeyInput = settingsDialog.getByLabel('API 密钥')
  await webKeyInput.fill('sk-e2e-web-secret')
  await settingsDialog.getByRole('button', { name: '保存配置' }).click()
  await settingsDialog.getByText('已配置密钥').waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal(await webKeyInput.inputValue(), '', 'Web search key input must clear after save')
  assert.equal(await page.getByText('sk-e2e-web-secret').count(), 0, 'Web search API key must stay redacted in UI')

  await settingsDialog.getByRole('button', { name: '自定义指令', exact: true }).click()
  const instructionsInput = settingsDialog.getByLabel('自定义指令内容')
  await instructionsInput.waitFor({ state: 'visible', timeout: 10_000 })
  await instructionsInput.fill('E2E_GLOBAL_INSTRUCTION_RULE')
  await settingsDialog.getByRole('button', { name: '保存', exact: true }).click()
  await settingsDialog.getByText('保存成功').waitFor({ state: 'visible', timeout: 10_000 })

  await settingsDialog.getByRole('button', { name: 'Skills', exact: true }).click()
  await settingsDialog.getByText('e2e-skill').waitFor({ state: 'visible', timeout: 10_000 })
  await settingsDialog.getByText('e2e-skill').click()
  await settingsDialog.getByRole('button', { name: '返回 Skills' }).waitFor({ state: 'visible', timeout: 10_000 })
  await settingsDialog.getByRole('button', { name: '返回 Skills' }).click()

  await settingsDialog.getByRole('button', { name: 'MCP', exact: true }).click()
  await settingsDialog.getByRole('heading', { name: /MCP/ }).first().waitFor({ state: 'visible', timeout: 10_000 })
  await settingsDialog.getByRole('button', { name: '关闭设置' }).click()
  await page.getByRole('switch', { name: /web_\*/ }).waitFor({ state: 'visible', timeout: 10_000 })
  await globalInstructionsCapsule.locator('.capsule-tokens').waitFor({ state: 'visible', timeout: 10_000 })
  assert.match(await globalInstructionsCapsule.getAttribute('title') ?? '', /AGENTS\.md/i)
}

async function main() {
  const mock = await startMockModelServer()
  const env = createIsolatedEnv(mock.baseUrl)
  let app = null
  try {
    console.log(
      packagedOnly
        ? `Running packaged E2E smoke: ${customExe}`
        : customExe
          ? `Running full desktop E2E smoke via executable: ${customExe}`
          : 'Running desktop E2E smoke suite...',
    )
    const launched = await launchApp(env.home)
    app = launched.app
    if (packagedOnly) {
      await runPackagedSmoke(launched.page, mock)
    } else {
      await runFullSmoke(launched.page, mock)
    }
    console.log('Desktop E2E smoke passed.')
  } finally {
    if (app) {
      await app.close().catch(() => {})
    }
    await mock.close().catch(() => {})
    env.cleanup()
  }
}

main().catch(error => {
  console.error('Desktop E2E smoke failed:', error)
  process.exit(1)
})
