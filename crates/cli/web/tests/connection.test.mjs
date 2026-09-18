import assert from 'node:assert/strict'
import { test } from 'node:test'
import { WorkbenchConnection } from '../src/connection.ts'
import { protocolVersion } from '../src/protocol.ts'
import { readyFrame } from './fixtures.ts'

test('connection keeps retrying after a long outage and stops cleanly', t => {
  const previousWindow = globalThis.window
  const previousSocket = globalThis.WebSocket
  const timers = new Map()
  const sockets = []
  const statuses = []
  const frames = []
  let timerId = 0
  globalThis.window = {
    location: { protocol: 'http:', host: '127.0.0.1:3081' },
    setTimeout: (callback, delay) => { timers.set(++timerId, { callback, delay }); return timerId },
    clearTimeout: id => timers.delete(id),
  }
  globalThis.WebSocket = class extends EventTarget {
    constructor() { super(); sockets.push(this) }
    close() { this.dispatchEvent(new Event('close')) }
  }
  t.after(() => { globalThis.window = previousWindow; globalThis.WebSocket = previousSocket })
  const connection = new WorkbenchConnection(frame => frames.push(frame), status => statuses.push(status))
  connection.start()
  for (let attempt = 0; attempt < 9; attempt++) {
    sockets.at(-1).close()
    assert.equal(timers.size, 1)
    const [id, timer] = [...timers][0]
    assert.ok(timer.delay <= 8000)
    timers.delete(id)
    timer.callback()
  }
  sockets.at(-1).dispatchEvent(new MessageEvent('message', { data: JSON.stringify(readyFrame()) }))
  assert.equal(frames.length, 1, 'ready frames reach the store for baseline sync')
  assert.equal(statuses.includes('ready'), false, 'the transport never claims application readiness')
  assert.equal(statuses.includes('unavailable'), false)
  sockets.at(-1).close()
  assert.equal(timers.size, 1)
  connection.stop()
  assert.equal(timers.size, 0)
})

test('RPC transport failure reconnects once and never replays the mutation', async t => {
  const previousWindow = globalThis.window
  const previousSocket = globalThis.WebSocket
  const previousFetch = globalThis.fetch
  const timers = []
  const sockets = []
  const statuses = []
  const frames = []
  let calls = 0
  globalThis.window = {
    location: { protocol: 'http:', host: '127.0.0.1:3081' },
    setTimeout: callback => { timers.push(callback); return timers.length },
    clearTimeout: () => {},
  }
  globalThis.WebSocket = class extends EventTarget {
    constructor() { super(); sockets.push(this) }
    close() { this.dispatchEvent(new Event('close')) }
  }
  globalThis.fetch = async () => { calls++; throw new Error('connection reset') }
  t.after(() => { globalThis.window = previousWindow; globalThis.WebSocket = previousSocket; globalThis.fetch = previousFetch })
  const connection = new WorkbenchConnection(frame => frames.push(frame), status => statuses.push(status))
  const ready = socket => socket.dispatchEvent(new MessageEvent('message', { data: JSON.stringify(readyFrame()) }))
  connection.start()
  ready(sockets[0])
  await assert.rejects(connection.rpc('session.submit', {workspaceId:'w',sessionId:'s',text:'hello'}), {code:'unavailable'})
  assert.equal(statuses.at(-1), 'recovering')
  assert.equal(timers.length, 1)
  ready(sockets[0])
  assert.equal(frames.length, 1, 'stale socket cannot restore readiness')
  timers.shift()()
  ready(sockets[1])
  assert.equal(statuses.at(-1), 'recovering', 'readiness is claimed by the store, not the transport')
  assert.equal(frames.length, 2, 'new ready frame triggers the usual baseline sync')
  assert.equal(calls, 1)
  connection.stop()
})

test('an unreadable or mismatched RPC response reconciles state once without replaying the mutation', async t => {
  const previousWindow = globalThis.window
  const previousSocket = globalThis.WebSocket
  const previousFetch = globalThis.fetch
  const timers = []
  const statuses = []
  let calls = 0
  globalThis.window = {
    location: { protocol: 'http:', host: '127.0.0.1:3081' },
    setTimeout: callback => { timers.push(callback); return timers.length },
    clearTimeout: () => {},
  }
  globalThis.WebSocket = class extends EventTarget {
    close() { this.dispatchEvent(new Event('close')) }
  }
  t.after(() => { globalThis.window = previousWindow; globalThis.WebSocket = previousSocket; globalThis.fetch = previousFetch })
  const connection = new WorkbenchConnection(() => {}, status => statuses.push(status))
  connection.start()
  // 三种「拿到了 HTTP 响应但结果不可信」的出口：body 读不出、请求标识不符、
  // 协议版本不符。变更可能已在服务端生效，所以都必须校准状态，但都不得重发。
  for (const envelope of [
    () => { throw new Error('truncated body') },
    () => ({ version: protocolVersion, requestId: 'someone-else', ok: true, result: null }),
    () => ({ version: protocolVersion + 1, requestId: 'someone-else', ok: true, result: null }),
  ]) {
    globalThis.fetch = async () => { calls++; return { status: 200, json: async () => envelope() } }
    await assert.rejects(connection.rpc('session.submit', { workspaceId: 'w', sessionId: 's', text: 'hello' }), { code: 'invalid_response' })
    assert.equal(statuses.at(-1), 'recovering', 'an uncertain result reuses the reconnect path')
    assert.equal(timers.length, 1, 'exactly one state reconciliation is scheduled')
    timers.shift()()
  }
  assert.equal(calls, 3, 'the mutation request is sent exactly once per attempt')
  connection.stop()
})
