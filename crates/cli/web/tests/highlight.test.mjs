import assert from 'node:assert/strict'
import { test } from 'node:test'
import { highlightCode } from '../src/highlight.ts'

// Shiki 4.4.3 是锁定依赖：应用只映射它的双主题输出，不再自己配对两套主题。
test('dual theme highlighting comes from the library as light and dark CSS variables', async () => {
  const lines = await highlightCode('const answer = 42', 'typescript')
  const tokens = lines.flat()
  assert.ok(tokens.length > 0)
  assert.equal(tokens.map(token => token.content).join(''), 'const answer = 42')
  for (const token of tokens) {
    assert.ok(token.htmlStyle, `every token carries library styles: ${JSON.stringify(token)}`)
    assert.ok('--shiki-light' in token.htmlStyle)
    assert.ok('--shiki-dark' in token.htmlStyle)
    assert.equal(token.color, undefined, 'defaultColor: false keeps colors in the variables')
  }
  const keyword = tokens.find(token => token.content === 'const')
  assert.notEqual(keyword.htmlStyle['--shiki-light'], keyword.htmlStyle['--shiki-dark'])
})

test('unknown languages fall back to plain text without failing', async () => {
  const lines = await highlightCode('not a known language', 'constructor')
  assert.equal(lines.flat().map(token => token.content).join(''), 'not a known language')
  assert.equal(lines.flat()[0].htmlStyle['--shiki-light'], undefined)
})

test('fence aliases and file extensions use the same highlighted language', async () => {
  for (const [alias, language, code] of [
    ['ts', 'typescript', 'const answer: number = 42'],
    ['js', 'javascript', 'const answer = 42'],
    ['sh', 'bash', 'echo "$HOME"'],
    ['rs', 'rust', 'let answer = 42;'],
    ['md', 'markdown', '# Title'],
    ['jsx', 'javascript', 'const answer = 42'],
  ]) {
    assert.deepEqual(await highlightCode(code, alias), await highlightCode(code, language), alias)
  }
})
