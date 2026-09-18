import assert from 'node:assert/strict'
import { test } from 'node:test'
import { readOutputLines } from '../src/readOutput'

type Source = { startLine: number; lineCount: number }
const numbers = (text: string, source: Source) => readOutputLines(text, source).map(line => line.number)
const body = (text: string, source: Source) => readOutputLines(text, source).map(line => line.text)

test('read line numbers come from the recorded source range, not from the tail text', () => {
  // 未截断输出：每行都是源文件行。
  assert.deepEqual(numbers('a\nb\nc', { startLine: 1, lineCount: 3 }), [1, 2, 3])
  // 尾部说明按位置排除：它在正文行数之外，不需要匹配任何文案。
  assert.deepEqual(numbers('a\nb\nc\n\n[Showing lines 1-3. File continues; use offset=4 to continue.]', { startLine: 1, lineCount: 3 }), [1, 2, 3, undefined, undefined])
  // 非 1 offset 的同一路径。
  assert.deepEqual(numbers('j\nk\nl\n\n[Showing lines 10-12. File continues; use offset=13 to continue.]', { startLine: 10, lineCount: 3 }), [10, 11, 12, undefined, undefined])
  // 源文件里恰好出现同样文字仍是正文行：判定不再依赖文案。
  const note = '[Showing lines 1-3. File continues; use offset=4 to continue.]'
  assert.deepEqual(numbers(`a\n${note}\nc`, { startLine: 1, lineCount: 3 }), [1, 2, 3])
  // 说明措辞变化不改变行号语义。
  assert.deepEqual(numbers('a\nb\nc\n\n[显示第 1-3 行，继续使用 offset=4。]', { startLine: 1, lineCount: 3 }), [1, 2, 3, undefined, undefined])
})

test('an oversized single line keeps its own number and nothing after it', () => {
  const text = 'x…[truncated]\n\n[Line 10 exceeds 32KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset=11.]'
  assert.deepEqual(numbers(text, { startLine: 10, lineCount: 1 }), [10, undefined, undefined])
  assert.deepEqual(body(text, { startLine: 10, lineCount: 1 }), ['x…[truncated]', '', '[Line 10 exceeds 32KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset=11.]'])
})

test('CRLF, blank content lines and a trailing newline keep file numbering', () => {
  assert.deepEqual(numbers('a\r\nb\r\nc\r\n\r\n[Showing lines 1-3. File continues; use offset=4 to continue.]', { startLine: 1, lineCount: 3 }), [1, 2, 3, undefined, undefined])
  // 正文里的空行仍是源文件行。
  assert.deepEqual(numbers('a\n\nc\n\n[Showing lines 4-6. File continues; use offset=7 to continue.]', { startLine: 4, lineCount: 3 }), [4, 5, 6, undefined, undefined])
  assert.deepEqual(numbers('a\nb\n', { startLine: 1, lineCount: 2 }), [1, 2])
})
