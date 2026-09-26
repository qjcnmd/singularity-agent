import { _electron as electron } from 'playwright'
import { existsSync, mkdirSync } from 'node:fs'
import { resolve, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import assert from 'node:assert/strict'
import { protocolVersion } from '../desktop-dist/src/protocol.generated.js'

export const modelSelector = process.env.SINGULARITY_E2E_SELECTOR ?? 'bai/deepseek-v4.1-flash'

/** 两条 E2E 共用启动边界；输出和每次启动的应用实例仍归各自场景。 */
export function setupE2E(outputName) {
  const home = process.env.SINGULARITY_HOME
  if (!home || !existsSync(join(home, 'config.json'))) {
    throw new Error('Set an isolated SINGULARITY_HOME and copy config.json/auth.json into it before running desktop E2E')
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
  const response = await page.evaluate(({ method, params, version }) => window.singularity.rpc({ version, method, params }), { method, params, version: protocolVersion })
  assert.equal(response.ok, true, JSON.stringify(response.error))
  return response.result
}
