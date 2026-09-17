import assert from 'node:assert/strict'
import { test } from 'node:test'
import { readOutputLines } from '../src/readOutput'

const numbers = (text: string, startLine: number) => readOutputLines(text, startLine).map(line => line.number)
const body = (text: string, startLine: number) => readOutputLines(text, startLine).map(line => line.text)

test('only the backend tail note is stripped from read line numbers', () => {
  // 未截断输出：每行都是源文件行。
  assert.deepEqual(numbers('a\nb\nc', 1), [1, 2, 3])
  // 标准分页截断：空行分隔与说明不带行号。
  assert.deepEqual(numbers('a\nb\nc\n\n[Showing lines 1-3. File continues; use offset=4 to continue.]', 1), [1, 2, 3, undefined, undefined])
  // 非 1 offset 的同一路径。
  assert.deepEqual(numbers('j\nk\nl\n\n[Showing lines 10-12. File continues; use offset=13 to continue.]', 10), [10, 11, 12, undefined, undefined])
})

test('a read tail note is recognized only as the complete, consistent tail', () => {
  const note = '[Showing lines 1-3. File continues; use offset=4 to continue.]'
  // 正文中出现的同一文字不是说明：它后面还有内容。
  assert.deepEqual(numbers(`a\n${note}\nc`, 1), [1, 2, 3])
  // 前缀位于首行、末行（无分隔空行）时同样按正文编号。
  assert.deepEqual(numbers(`${note}\na\nb`, 1), [1, 2, 3])
  assert.deepEqual(numbers(`a\nb\n${note}`, 1), [1, 2, 3])
  // 编号与 offset/正文行数不一致：不认作说明。
  assert.deepEqual(numbers('a\nb\nc\n\n[Showing lines 1-9. File continues; use offset=10 to continue.]', 1), [1, 2, 3, 4, 5])
  assert.deepEqual(numbers('a\nb\nc\n\n[Showing lines 3-5. File continues; use offset=6 to continue.]', 1), [1, 2, 3, 4, 5])
})

test('an oversized single line keeps its own number and nothing after it', () => {
  const text = 'x…[truncated]\n\n[Line 10 exceeds 32KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset=11.]'
  assert.deepEqual(numbers(text, 10), [10, undefined, undefined])
  assert.deepEqual(body(text, 10), ['x…[truncated]', '', '[Line 10 exceeds 32KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset=11.]'])
  // 同一说明配多行正文不成立：后端只在正文仅一行时使用它。
  assert.deepEqual(numbers(`a\nb\nc\n\n[Line 1 exceeds 32KB; only its prefix is shown. Use bash to read this line in byte ranges. For following lines use offset=2.]`, 1), [1, 2, 3, 4, 5])
})

test('CRLF, blank content lines and a trailing newline keep file numbering', () => {
  assert.deepEqual(numbers('a\r\nb\r\nc\r\n\r\n[Showing lines 1-3. File continues; use offset=4 to continue.]', 1), [1, 2, 3, undefined, undefined])
  // 正文里的空行仍是源文件行。
  assert.deepEqual(numbers('a\n\nc\n\n[Showing lines 4-6. File continues; use offset=7 to continue.]', 4), [4, 5, 6, undefined, undefined])
  assert.deepEqual(numbers('a\nb\n', 1), [1, 2])
})
