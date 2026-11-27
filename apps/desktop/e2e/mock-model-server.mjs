import { createServer } from 'node:http'

function sse(payload) {
  return `data: ${JSON.stringify(payload)}\n\n`
}

export function messagesTextEvents({ thinking, text, inputTokens = 100, outputTokens = 40, stopReason = 'end_turn' }) {
  const events = [
    sse({ type: 'message_start', message: { usage: { input_tokens: inputTokens, output_tokens: 1 } } }),
  ]
  let index = 0
  if (thinking) {
    events.push(sse({ type: 'content_block_start', index, content_block: { type: 'thinking', thinking: '', signature: '' } }))
    events.push(sse({ type: 'content_block_delta', index, delta: { type: 'thinking_delta', thinking } }))
    events.push(sse({ type: 'content_block_delta', index, delta: { type: 'signature_delta', signature: 'sig-e2e' } }))
    events.push(sse({ type: 'content_block_stop', index }))
    index += 1
  }
  if (text) {
    events.push(sse({ type: 'content_block_start', index, content_block: { type: 'text', text: '' } }))
    events.push(sse({ type: 'content_block_delta', index, delta: { type: 'text_delta', text } }))
    events.push(sse({ type: 'content_block_stop', index }))
  }
  events.push(sse({ type: 'message_delta', delta: { stop_reason: stopReason }, usage: { output_tokens: outputTokens } }))
  events.push(sse({ type: 'message_stop' }))
  return events
}

export function messagesToolEvents({ calls, inputTokens = 100, outputTokens = 40 }) {
  const events = [
    sse({ type: 'message_start', message: { usage: { input_tokens: inputTokens, output_tokens: 1 } } }),
  ]
  let index = 0
  for (const { id, name, args } of calls) {
    events.push(sse({ type: 'content_block_start', index, content_block: { type: 'tool_use', id, name, input: {} } }))
    events.push(sse({ type: 'content_block_delta', index, delta: { type: 'input_json_delta', partial_json: JSON.stringify(args) } }))
    events.push(sse({ type: 'content_block_stop', index }))
    index += 1
  }
  events.push(sse({ type: 'message_delta', delta: { stop_reason: 'tool_use' }, usage: { output_tokens: outputTokens } }))
  events.push(sse({ type: 'message_stop' }))
  return events
}

export function createGate() {
  let release
  const promise = new Promise(resolve => { release = resolve })
  return { promise, release }
}

export async function startMockModelServer() {
  const queue = []
  const captured = []

  const server = createServer(async (req, res) => {
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    const serialized = Buffer.concat(chunks).toString('utf8')
    let body = null
    if (serialized) {
      try {
        body = JSON.parse(serialized)
      } catch {
        body = null
      }
    }
    captured.push({ method: req.method, url: req.url, raw: serialized, body })

    let entryIdx = queue.findIndex(item => item.matchText && serialized.includes(item.matchText))
    if (entryIdx < 0) entryIdx = queue.findIndex(item => !item.matchText)
    const entry = entryIdx >= 0 ? queue.splice(entryIdx, 1)[0] : null
    if (!entry) {
      res.writeHead(500, { 'Content-Type': 'application/json' })
      res.end(JSON.stringify({ error: { message: 'unexpected E2E model request' } }))
      return
    }
    if (entry.jsonError) {
      res.writeHead(entry.status ?? 400, { 'Content-Type': 'application/json' })
      res.end(JSON.stringify(entry.jsonError))
      return
    }

    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache',
      Connection: 'close',
    })
    const gates = entry.gates ?? []
    for (let i = 0; i < entry.events.length; i += 1) {
      if (res.writableEnded || res.destroyed) break
      await new Promise(resolve => res.write(entry.events[i], resolve))
      for (const step of gates) {
        if (step.afterIndex === i) {
          step.onReached?.()
          await Promise.race([
            step.gate.promise,
            new Promise(resolve => req.once('close', resolve)),
          ])
        }
      }
    }
    if (!res.writableEnded) res.end()
  })

  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve))
  const address = server.address()
  return {
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    enqueue(events, options = {}) { queue.push({ events, ...options }) },
    enqueueError(status, jsonError, options = {}) { queue.push({ status, jsonError, ...options }) },
    requests() { return [...captured] },
    pendingCount() { return queue.length },
    assertQueueDrained() {
      if (queue.length > 0) {
        const summary = queue.map((item, idx) => `#${idx}:${item.matchText ?? (item.jsonError ? `err:${item.status}` : 'sse')}`)
        throw new Error(`mock model server still has ${queue.length} unconsumed response(s): ${summary.join(', ')}`)
      }
    },
    async close() {
      server.closeAllConnections?.()
      await new Promise(resolve => server.close(resolve))
    },
  }
}
