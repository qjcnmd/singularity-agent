import assert from 'node:assert/strict'
import { test } from 'node:test'
import { readFileSync, writeFileSync, unlinkSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'

test('actual Rust serialization satisfies client events, frames and RPC results', () => {
  const fixtureRoot = new URL('../../../protocol/tests/fixtures/', import.meta.url)
  const events = readFileSync(new URL('turn-events.json', fixtureRoot), 'utf8')
  const frames = readFileSync(new URL('stream-frames.json', fixtureRoot), 'utf8')
  const rpc = JSON.parse(readFileSync(new URL('rpc.json', fixtureRoot), 'utf8'))
  assert.equal(JSON.parse(frames).length, 6)
  const path = new URL(`./.protocol-contract-${process.pid}.ts`, import.meta.url)
  try {
    writeFileSync(path, `import type { TurnEventEnvelope, StreamEnvelope, RpcResponse, RpcParams } from '../src/protocol'
const events = ${events.trimEnd()} satisfies TurnEventEnvelope[]
const frames = ${frames.trimEnd()} satisfies StreamEnvelope[]
const params = ${JSON.stringify(rpc.request.params)} satisfies RpcParams<'workbench.bootstrap'>
const success = ${JSON.stringify(rpc.success)} satisfies RpcResponse<'workbench.bootstrap'>
const failure = ${JSON.stringify(rpc.failure)} satisfies RpcResponse<'workbench.bootstrap'>
`)
    execFileSync(process.execPath, [fileURLToPath(new URL('../node_modules/typescript/bin/tsc', import.meta.url)),
      '--ignoreConfig', '--noEmit', '--strict', '--skipLibCheck', '--target', 'esnext', '--module', 'esnext',
      '--moduleResolution', 'bundler', fileURLToPath(path)], { encoding: 'utf8' })
  } finally { unlinkSync(path) }
})
