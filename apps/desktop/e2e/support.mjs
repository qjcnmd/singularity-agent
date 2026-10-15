import { _electron as electron } from 'playwright'
import { existsSync, mkdirSync } from 'node:fs'
import { resolve, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import assert from 'node:assert/strict'

export const modelSelector = process.env.SINGULARITY_E2E_SELECTOR ?? 'bai/deepseek-v4.1-flash'

/** 桌面 E2E 共用启动边界；输出和每次启动的应用实例仍归各自场景。 */
export function setupE2E(outputName) {
  const home = process.env.SINGULARITY_HOME
  if (!home || !existsSync(join(home, 'config.json'))) {
    throw new Error('Set SINGULARITY_HOME to a data directory containing config.json and auth.json (normally $HOME/.singularity) before running desktop E2E')
  }
  const desktop = resolve(fileURLToPath(new URL('..', import.meta.url)))
  const output = resolve(process.env.SINGULARITY_E2E_OUTPUT ?? join(desktop, '../../outputs', outputName))
  mkdirSync(output, { recursive: true })
  const env = { ...process.env }
  delete env.ELECTRON_RUN_AS_NODE
  const options = { ...(env.SINGULARITY_E2E_PACKAGED ? { executablePath: env.SINGULARITY_E2E_PACKAGED, args: [] } : { args: [desktop] }), env, timeout: 60_000 }
  return { desktop, output, env, launch: () => electron.launch(options) }
}

export async function rpc(page, method, params = {}) {
  const response = await page.evaluate(({ method, params }) => window.singularity.rpc({ method, params }), { method, params })
  assert.equal(response.type, 'success', JSON.stringify(response.error))
  return response.result
}

const workspaceRoot = root => resolve(root.replaceAll('\\', '/').replace(/^\/\/\?\//, '')).toLowerCase()

/** 按完整目录识别项目，同名项目可以同时存在。 */
export async function findWorkspace(page, root) {
  const target = workspaceRoot(root)
  return (await rpc(page, 'app.bootstrap')).workspaces.find(workspace => workspaceRoot(workspace.root) === target)
}

/** 重跑测试时复用已登记的项目，保留已有任务和草稿。 */
export async function ensureWorkspace(page, root) {
  mkdirSync(root, { recursive: true })
  return await findWorkspace(page, root) ?? await rpc(page, 'workspace.add', { root })
}

export function workspaceNewSessionButton(page, workspace) {
  return page.getByTitle(workspace.root, { exact: true }).locator('..')
    .getByRole('button', { name: `在 ${workspace.name} 新建任务`, exact: true })
}
