import assert from 'node:assert/strict'
import { test } from 'node:test'
import { sessionTitles } from '../src/sessionTitle'
import { summary } from './fixtures'

test('untitled task numbers come from one group-level derivation', () => {
  // 无任务：没有可读取的标题，但派生本身不报错。
  assert.equal(sessionTitles([])(summary()), '新任务')

  // 单个未命名任务不编号。
  const one = summary({ threadId: 'a' })
  assert.equal(sessionTitles([one])(one), '新任务')

  // 多个未命名任务按创建时间、再按 ID 排序编号。
  const first = summary({ threadId: 'a', createdAt: '2026-09-01T00:00:00Z' })
  const second = summary({ threadId: 'b', createdAt: '2026-09-02T00:00:00Z' })
  const tie = summary({ threadId: 'c', createdAt: '2026-09-02T00:00:00Z' })
  const group = sessionTitles([second, tie, first])
  assert.equal(group(first), '新任务 1')
  assert.equal(group(second), '新任务 2')
  assert.equal(group(tie), '新任务 3')

  // 用户命名优先，并保留修剪规则。
  const named = summary({ threadId: 'a', title: '  已命名  ' })
  assert.equal(sessionTitles([named, second])(named), '已命名')
  // 空白标题仍算未命名。
  const blank = summary({ threadId: 'a', title: '   ', createdAt: '2026-09-01T00:00:00Z' })
  assert.equal(sessionTitles([blank, second])(blank), '新任务 1')

  // 不在组内的任务按同一规则取名，不虚构序号。
  assert.equal(group(summary({ threadId: 'outside' })), '新任务')
  assert.equal(group(summary({ threadId: 'outside', title: '外部' })), '外部')
})

test('visibility filtering never changes a task title', () => {
  // 侧栏隐藏空白任务后仍以完整组派生；页头读同一份结果。
  const hiddenBlank = summary({ threadId: 'blank', turnCount: 0, status: null, title: null, createdAt: '2026-09-01T00:00:00Z' })
  const visible = summary({ threadId: 'visible', createdAt: '2026-09-02T00:00:00Z' })
  const titles = sessionTitles([hiddenBlank, visible])
  assert.equal(titles(visible), '新任务 2')
  // 只把可见行传给派生（错误做法）会得到“新任务”，两处显示因此不一致。
  assert.equal(sessionTitles([visible])(visible), '新任务')
  assert.equal(titles(hiddenBlank), '新任务 1')
})
