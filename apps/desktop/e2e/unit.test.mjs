import assert from 'node:assert/strict'
import { describe, it } from 'node:test'
import { sessionTitles } from '../src/sessionTitle.ts'
import { latestTodos, parseTodos, todoSummary } from '../src/todos.ts'
import { displayPath, resolveFileUrl, workspaceRelativePath } from '../src/workspace.ts'

describe('sessionTitles', () => {
  it('numbers multiple untitled sessions chronologically and preserves explicit titles', () => {
    const sessions = [
      { threadId: 'b', cwd: 'C:/repo', createdAt: '2026-01-02T00:00:00Z', updatedAt: '', title: null, status: null, manuallyStopped: false },
      { threadId: 'a', cwd: 'C:/repo', createdAt: '2026-01-01T00:00:00Z', updatedAt: '', title: '   ', status: null, manuallyStopped: false },
      { threadId: 'c', cwd: 'C:/repo', createdAt: '2026-01-03T00:00:00Z', updatedAt: '', title: 'Custom', status: null, manuallyStopped: false },
    ]
    const label = sessionTitles(sessions)
    assert.equal(label(sessions[1]), '新任务 1')
    assert.equal(label(sessions[0]), '新任务 2')
    assert.equal(label(sessions[2]), 'Custom')
  })
})

describe('todos', () => {
  it('extracts the latest completed non-error todo snapshot and summarizes progress', () => {
    assert.equal(parseTodos({ todos: 'invalid' }), null)
    const turns = [
      {
        id: 't1',
        startedAt: '',
        items: [
          { type: 'tool', id: '1', name: 'todo', status: 'completed', args: { todos: [{ content: 'Step 1', status: 'completed' }, { content: 'Step 2', status: 'in_progress' }] }, result: { isError: false } },
          { type: 'tool', id: '2', name: 'todo', status: 'completed', args: { todos: [{ content: 'Broken', status: 'pending' }] }, result: { isError: true } },
        ],
      },
    ]
    const items = latestTodos(turns)
    assert.deepEqual(items, [
      { content: 'Step 1', status: 'completed' },
      { content: 'Step 2', status: 'in_progress' },
    ])
    assert.equal(todoSummary(items), '1/2 · Step 2')
    assert.equal(todoSummary([]), '已清空')
  })
})

describe('workspace paths', () => {
  it('normalizes Windows paths, rejects traversal, and encodes file URLs', () => {
    assert.equal(workspaceRelativePath('c:\\Repo\\src\\a.ts', 'C:/repo'), 'src/a.ts')
    assert.equal(workspaceRelativePath('../outside.ts', 'C:/repo'), null)
    assert.equal(workspaceRelativePath('D:/other/a.ts', 'C:/repo'), null)
    assert.equal(displayPath('C:\\Users\\Me\\file.txt', 'C:/repo', 'C:\\Users\\Me'), '~/file.txt')
    assert.equal(resolveFileUrl('docs/a#b?c.md', 'C:/repo')?.href, 'file:///C:/repo/docs/a%23b%3Fc.md')
  })
})
