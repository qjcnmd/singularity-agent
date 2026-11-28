import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { _electron as electron } from 'playwright-core'

import {
  createGate,
  messagesTextEvents,
  messagesToolEvents,
  startMockModelServer,
} from './mock-model-server.mjs'

const require = createRequire(import.meta.url)
const electronExecutable = require('electron')

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

  const runGit = args => execFileSync('git', args, { cwd: workspace, stdio: 'ignore' })
  runGit(['init', '-q'])
  runGit(['config', 'user.email', 'e2e@example.invalid'])
  runGit(['config', 'user.name', 'E2E'])
  runGit(['add', '-A'])
  runGit(['commit', '-q', '-m', 'baseline'])

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
                max_context_tokens: 16000,
                max_output_tokens: 2048,
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

async function launchWindow(home, openNewTask = true) {
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
  if (openNewTask) {
    await page.getByRole('button', { name: /在 workspace 新建任务/ }).click()
    await page.getByLabel('任务说明').waitFor({ state: 'visible', timeout: 20_000 })
  }
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

async function waitForDraftPersisted(page, expectedText) {
  await page.waitForFunction(
    text => new Promise(resolve => {
      const req = indexedDB.open('singularity.drafts', 1)
      req.onerror = () => resolve(false)
      req.onsuccess = () => {
        const db = req.result
        if (!db.objectStoreNames.contains('drafts')) {
          db.close()
          resolve(false)
          return
        }
        const tx = db.transaction('drafts', 'readonly')
        const getAll = tx.objectStore('drafts').getAll()
        getAll.onsuccess = () => {
          const found = (getAll.result || []).some(draft => draft && draft.text === text)
          db.close()
          resolve(found)
        }
        getAll.onerror = () => {
          db.close()
          resolve(false)
        }
      }
    }),
    expectedText,
    { timeout: 10_000 },
  )
}

async function selectAssistantPhrase(page, phrase) {
  const selected = await page.evaluate(target => {
    const bodies = [...document.querySelectorAll('[data-annotatable-message-id]')]
    for (const body of bodies) {
      const walker = document.createTreeWalker(body, NodeFilter.SHOW_TEXT)
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const text = node.nodeValue ?? ''
        const idx = text.indexOf(target)
        if (idx === -1) continue
        const parent = node.parentElement
        if (!parent) continue
        parent.scrollIntoView({ block: 'center', behavior: 'instant' })
        const range = document.createRange()
        range.setStart(node, idx)
        range.setEnd(node, idx + target.length)
        const selection = window.getSelection()
        selection?.removeAllRanges()
        selection?.addRange(range)
        parent.dispatchEvent(new PointerEvent('pointerup', { bubbles: true }))
        return true
      }
    }
    return false
  }, phrase)
  assert.ok(selected, `could not locate annotatable assistant phrase: ${phrase}`)
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
  mock.assertQueueDrained()
}

async function runStage1RichRendering(page, mock) {
  console.log('  [1/5] rich rendering (KaTeX + Mermaid), TurnChangesCard, Changes diff & reload recovery')
  await page.emulateMedia({ reducedMotion: 'reduce' })
  mock.enqueue(
    messagesToolEvents({
      calls: [
        { id: 'read_1', name: 'read', args: { path: 'readme.md' } },
        {
          id: 'edit_1',
          name: 'edit',
          args: {
            path: 'readme.md',
            edits: [{ old_text: 'WORKSPACE_FILE_BODY', new_text: 'WORKSPACE_FILE_UPDATED_E2E' }],
          },
        },
        { id: 'write_1', name: 'write', args: { path: 'notes.txt', content: 'E2E_NEW_NOTE_LINE\n' } },
        {
          id: 'todo_1',
          name: 'todo',
          args: {
            todos: [
              { content: 'Audit E2E step', status: 'completed' },
              { content: 'Verify RunPanel', status: 'in_progress' },
            ],
          },
        },
      ],
    }),
  )
  const richMarkdown = [
    'RICH_RENDER_MARKER',
    '',
    'ANNOTATE_TARGET_PHRASE for reply annotation testing.',
    '',
    '行内公式：$E = mc^2$',
    '',
    '```mermaid',
    'flowchart LR',
    '  A[Start] --> B[Verified]',
    '```',
    '',
    '查看文件：`readme.md`',
    '',
    'COMPACTION_PREFIX_PAD '.repeat(60),
  ].join('\n')
  mock.enqueue(
    messagesTextEvents({
      thinking: 'checking readme before answering',
      text: richMarkdown,
    }),
  )

  await submitPrompt(page, 'inspect readme and render diagram')
  await page.getByText('RICH_RENDER_MARKER').waitFor({ state: 'visible', timeout: 20_000 })
  await waitForIdle(page)
  await page.locator('.katex').first().waitFor({ state: 'visible', timeout: 15_000 })
  await page.locator('.mermaid-diagram svg').first().waitFor({ state: 'visible', timeout: 15_000 })
  const mermaidBox = await page.locator('.mermaid-diagram svg').first().boundingBox()
  assert.ok(mermaidBox && mermaidBox.width > 40 && mermaidBox.height > 20, 'Mermaid SVG must have non-zero size under reducedMotion')

  await page.getByRole('link', { name: 'readme.md' }).first().click()
  const filePanel = page.getByRole('complementary', { name: '文件' })
  await filePanel.waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.getByText('WORKSPACE_FILE_UPDATED_E2E').waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.getByRole('button', { name: '关闭 readme.md' }).click()
  await filePanel.waitFor({ state: 'hidden', timeout: 10_000 })

  const turnChanges = page.locator('.turn-changes-card').last()
  await turnChanges.waitFor({ state: 'visible', timeout: 10_000 })
  await turnChanges.locator('.turn-changes-row[title="readme.md"]').click()
  await filePanel.waitFor({ state: 'visible', timeout: 10_000 })
  const changesTab = filePanel.getByRole('tab', { name: 'Changes' })
  await changesTab.waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal(await changesTab.getAttribute('aria-selected'), 'true')
  const readmeDiff = filePanel.getByRole('region', { name: 'readme.md 的代码差异' })
  await readmeDiff.waitFor({ state: 'visible', timeout: 10_000 })
  await readmeDiff.getByText('WORKSPACE_FILE_UPDATED_E2E').waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.locator('.changes-row[title="notes.txt"]').click()
  const notesDiff = filePanel.getByRole('region', { name: 'notes.txt 的代码差异' })
  await notesDiff.waitFor({ state: 'visible', timeout: 10_000 })
  await notesDiff.getByText('E2E_NEW_NOTE_LINE').waitFor({ state: 'visible', timeout: 10_000 })
  await filePanel.getByRole('button', { name: '关闭 Changes' }).click()
  await filePanel.waitFor({ state: 'hidden', timeout: 10_000 })

  await page.reload()
  await page.getByText('RICH_RENDER_MARKER').waitFor({ state: 'visible', timeout: 20_000 })
}

async function runStage2AnnotationAndRewind(page, mock) {
  console.log('  [2/5] assistant reply annotation & inline user message edit/rewind')
  await selectAssistantPhrase(page, 'ANNOTATE_TARGET_PHRASE')
  const annotateBtn = page.locator('.annotation-trigger-button')
  await annotateBtn.waitFor({ state: 'visible', timeout: 10_000 })
  await annotateBtn.click()
  const annotateDialog = page.getByRole('dialog', { name: '添加批注' })
  await annotateDialog.waitFor({ state: 'visible', timeout: 10_000 })
  await annotateDialog.getByLabel('批注内容').fill('E2E_ANNOTATION_COMMENT')
  await annotateDialog.getByRole('button', { name: '保存批注' }).click()
  const draftTray = page.locator('.draft-annotations')
  await draftTray.waitFor({ state: 'visible', timeout: 10_000 })
  await draftTray.getByText('1 条批注').waitFor({ state: 'visible', timeout: 10_000 })

  mock.enqueue(messagesTextEvents({ text: 'ORIGINAL_REPLY_TO_REPLACE' }))
  await submitPrompt(page, 'follow up with annotation')
  await page.getByText('ORIGINAL_REPLY_TO_REPLACE').waitFor({ state: 'visible', timeout: 15_000 })
  await waitForIdle(page)
  const annotatedReq = mock.requests().at(-1)?.raw ?? ''
  assert.ok(
    annotatedReq.includes('<annotations>')
      && annotatedReq.includes('ANNOTATE_TARGET_PHRASE')
      && annotatedReq.includes('E2E_ANNOTATION_COMMENT'),
    'upstream model request must include quote and comment from reply annotation',
  )
  assert.equal(await draftTray.count(), 0, 'draft annotations must clear after submit')
  await page.locator('.sent-annotations').last().waitFor({ state: 'visible', timeout: 10_000 })

  await page.getByRole('button', { name: '编辑消息' }).last().click()
  const editInput = page.getByLabel('编辑消息内容')
  await editInput.waitFor({ state: 'visible', timeout: 10_000 })
  await editInput.fill('edited user prompt after rewind')
  const longRetainedTail = `EDITED_REWIND_REPLY_OK\n\n${'compaction tail segment '.repeat(400)}`
  mock.enqueue(messagesTextEvents({ text: longRetainedTail }))
  await page.getByRole('button', { name: '发送', exact: true }).click()
  await page.getByText('EDITED_REWIND_REPLY_OK').waitFor({ state: 'visible', timeout: 15_000 })
  await waitForIdle(page)
  assert.equal(await page.getByText('ORIGINAL_REPLY_TO_REPLACE').count(), 0, 'rewound old reply must be replaced')

  await page.reload()
  await page.getByText('EDITED_REWIND_REPLY_OK').waitFor({ state: 'visible', timeout: 20_000 })
  await page.getByText('edited user prompt after rewind').waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal(await page.getByText('ORIGINAL_REPLY_TO_REPLACE').count(), 0)
}

async function runStage3RestartAndRunPanel(ctx, env, mock) {
  console.log('  [3/5] cold restart & RunPanel (Context lazy load, Todos, Compact -> Confirm)')
  await ctx.app.close()
  const restarted = await launchWindow(env.home, false)
  ctx.app = restarted.app
  ctx.page = restarted.page
  const { page } = ctx
  await page.emulateMedia({ reducedMotion: 'reduce' })
  await page.locator('.session-main').first().click()
  await page.getByText('EDITED_REWIND_REPLY_OK').waitFor({ state: 'visible', timeout: 20_000 })

  await page.getByRole('button', { name: '运行状态面板' }).click()
  const runPanel = page.getByRole('complementary', { name: '运行状态' })
  await runPanel.waitFor({ state: 'visible', timeout: 10_000 })

  const todoList = runPanel.getByRole('list', { name: '待办清单' })
  await todoList.waitFor({ state: 'visible', timeout: 10_000 })
  await todoList.getByText('Audit E2E step').waitFor({ state: 'visible', timeout: 10_000 })
  await todoList.getByText('Verify RunPanel').waitFor({ state: 'visible', timeout: 10_000 })
  await runPanel.getByText('1/2').waitFor({ state: 'visible', timeout: 10_000 })

  await runPanel.locator('.run-row-toggle', { hasText: 'Context' }).click()
  const contextPanel = runPanel.locator('.context-panel')
  await contextPanel.waitFor({ state: 'visible', timeout: 15_000 })
  const toolsGroup = contextPanel.getByRole('button', { name: /Tools/ })
  await toolsGroup.waitFor({ state: 'visible', timeout: 10_000 })
  await toolsGroup.click()
  await contextPanel.locator('.context-item-row').first().waitFor({ state: 'visible', timeout: 10_000 })

  const compactBtn = runPanel.locator('.compact-button')
  await compactBtn.waitFor({ state: 'visible', timeout: 10_000 })
  assert.equal((await compactBtn.textContent())?.trim(), 'Compact')
  await compactBtn.click()
  await runPanel.getByRole('button', { name: 'Confirm compact context' }).waitFor({ state: 'visible', timeout: 10_000 })
  await runPanel.locator('.context-usage-text').click()
  await runPanel.getByRole('button', { name: /Compact context/ }).waitFor({ state: 'visible', timeout: 10_000 })

  const cardsBeforeCompact = await page.locator('.activity-card').count()
  mock.enqueue(messagesTextEvents({ text: 'E2E_COMPACTED_CHECKPOINT_OK' }))
  await compactBtn.click()
  await runPanel.getByRole('button', { name: 'Confirm compact context' }).click()
  await page.waitForFunction(
    prevCount => document.querySelectorAll('.activity-card').length > prevCount,
    cardsBeforeCompact,
    { timeout: 15_000 },
  )
  await waitForIdle(page)
  await page.keyboard.press('Escape')
  await runPanel.waitFor({ state: 'hidden', timeout: 10_000 })
  await page.locator('.activity-card').last().locator('.activity-card-header').click()
  await page.getByText('上下文已压缩').waitFor({ state: 'visible', timeout: 10_000 })
}

async function runStage4MultiSessionAndRecovery(page, mock) {
  console.log('  [4/5] multi-session isolation, stop, question panel, error recovery & session delete')
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
  await page.evaluate(async () => {
    await window.desktop.rpc({ method: 'model.read', params: {} })
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))
  })
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
}

async function runStage5PersistenceAndSettings(page) {
  console.log('  [5/5] draft & theme persistence, settings provider / web search / instructions / skills / mcp')
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  const composer = page.getByLabel('任务说明')
  await composer.fill('UNSENT_DRAFT_SURVIVES_RELOAD')
  await waitForDraftPersisted(page, 'UNSENT_DRAFT_SURVIVES_RELOAD')
  await page.locator('.session-main').first().click()
  await page.getByRole('button', { name: '新建任务', exact: true }).click()
  assert.equal(await composer.inputValue(), 'UNSENT_DRAFT_SURVIVES_RELOAD')

  await page.getByRole('button', { name: '设置' }).click()
  await page.getByRole('radio', { name: '深色' }).click()
  await page.getByRole('button', { name: '关闭设置' }).click()
  await page.reload()
  await page.getByLabel('任务说明').waitFor({ state: 'visible', timeout: 20_000 })
  await page.waitForFunction(
    expected => {
      const el = document.querySelector('textarea[aria-label="任务说明"]')
      return el instanceof HTMLTextAreaElement && el.value === expected
    },
    'UNSENT_DRAFT_SURVIVES_RELOAD',
    { timeout: 10_000 },
  )
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'dark')

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

async function runFullSmoke(initialApp, initialPage, env, mock) {
  const ctx = { app: initialApp, page: initialPage }
  try {
    await runStage1RichRendering(ctx.page, mock)
    await runStage2AnnotationAndRewind(ctx.page, mock)
    await runStage3RestartAndRunPanel(ctx, env, mock)
    await runStage4MultiSessionAndRecovery(ctx.page, mock)
    await runStage5PersistenceAndSettings(ctx.page)
    mock.assertQueueDrained()
    return ctx.app
  } catch (error) {
    await ctx.app.close().catch(() => {})
    throw error
  }
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
    const launched = await launchWindow(env.home, true)
    app = launched.app
    if (packagedOnly) {
      await runPackagedSmoke(launched.page, mock)
    } else {
      app = await runFullSmoke(launched.app, launched.page, env, mock)
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
