import assert from 'node:assert/strict'
import { test } from 'node:test'
import { WorkbenchConnection } from '../src/connection.ts'
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
