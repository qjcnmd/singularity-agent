import assert from 'node:assert/strict'
import { test } from 'node:test'
import { acceptExecutionEvent } from '../src/execution.ts'

test('完成的回复按保存顺序替换增量；工具更新和再次读取不改变位置', () => {
  const event = (method, params) => ({ turnId: 'turn', phase: 'running', method, params })
  const user = { type: 'message', id: 'user:text:0', role: 'user', text: '检查文件', status: 'completed' }
  const started = event('turn/started', { turn: { turnId: 'turn', status: 'running' }, startedAt: '2026-10-23T00:00:00Z' })
  let facts = acceptExecutionEvent([], started)
  facts = acceptExecutionEvent(facts, event('item/updated', { item: user }))
  // 历史可能先包含开始记录，随后才收到开始事件；保留已读到的内容。
  facts = acceptExecutionEvent(facts, started)
  assert.equal(facts.length, 1)
  assert.deepEqual(facts[0].items, [user])
  for (const [thinking, delta] of [[false, '先读文件，然后说明'], [true, '思考']]) {
    facts = acceptExecutionEvent(facts, event('item/textDelta', {
      itemId: `reply:${thinking ? 'thinking' : 'text'}:0`, thinking, delta,
    }))
  }
  const saved = [
    { type: 'thinking', id: 'reply:thinking:0', text: '思考', status: 'completed' },
    { type: 'message', id: 'reply:text:0', role: 'assistant', text: '先读文件', status: 'completed' },
    { type: 'tool', id: 'reply:tool:0', name: 'read', args: { path: 'README.md' }, result: null, status: 'pending' },
    { type: 'message', id: 'reply:text:1', role: 'assistant', text: '然后说明', status: 'completed' },
  ]
  const completed = event('message/finished', { messageId: 'reply', items: saved })
  facts = acceptExecutionEvent(facts, completed)
  assert.deepEqual(facts[0].items, [user, ...saved])
  // 读取可能已经包含刚保存的消息，随后才收到它的完成事件。
  facts = acceptExecutionEvent(facts, completed)
  assert.deepEqual(facts[0].items, [user, ...saved])
  facts = acceptExecutionEvent(facts, event('item/updated', { item: { ...saved[2], status: 'running' } }))
  facts = acceptExecutionEvent(facts, event('tool/finished', {
    itemId: 'reply:tool:0', result: { output: '文件内容', isError: false },
  }))
  assert.deepEqual(facts[0].items.map(item => item.id), [user, ...saved].map(item => item.id))
  assert.equal(facts[0].items[3].result.output, '文件内容')
  facts = acceptExecutionEvent(facts, event('item/textDelta', { itemId: 'failed:text:0', thinking: false, delta: '未完成' }))
  facts = acceptExecutionEvent(facts, event('message/finished', { messageId: 'failed', items: [] }))
  assert.equal(facts[0].items.some(item => item.id === 'failed:text:0'), false)
})

test('八个工具运行时第九个仍在等待，停止后不再显示等待', () => {
  const event = (method, params) => ({ turnId: 'turn', phase: 'running', method, params })
  const tools = Array.from({ length: 9 }, (_, index) => ({
    type: 'tool', id: `reply:tool:${index}`, name: 'read', args: {}, result: null, status: 'pending',
  }))
  let facts = acceptExecutionEvent([], event('turn/started', {
    turn: { turnId: 'turn', status: 'running' }, startedAt: '2026-10-23T00:00:00Z',
  }))
  facts = acceptExecutionEvent(facts, event('message/finished', { messageId: 'reply', items: tools }))
  for (const tool of tools.slice(0, 8)) {
    facts = acceptExecutionEvent(facts, event('item/updated', { item: { ...tool, status: 'running' } }))
  }
  assert.deepEqual(facts[0].items.map(item => item.status), [...Array(8).fill('running'), 'pending'])
  facts = acceptExecutionEvent(facts, event('turn/completed', {
    turn: { turnId: 'turn', status: 'interrupted' }, manuallyStopped: true,
  }))
  assert.ok(facts[0].items.every(item => item.status === 'interrupted'))
})
