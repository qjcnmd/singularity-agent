import assert from 'node:assert/strict'
import { test } from 'node:test'
import { sessionState } from '../src/sessionState'
import { summary, startedAt } from './fixtures'
import type { LiveSessionState } from '../src/sync'

const live = (
  source: 'turn' | 'compaction' | null,
  status: 'completed' | 'failed' | 'interrupted',
  phase: LiveSessionState['phase'] = 'idle',
): LiveSessionState => ({
  sessionRevision: 1,
  phase,
  terminal: source === null ? null : { source, status, message: null },
})

test('a standalone compaction outcome never marks the task failed', () => {
  const task = summary({ status: 'completed' })

  // 回合终态描述任务本身：失败与停止都跟随。
  assert.deepEqual(sessionState(task, live('turn', 'failed')), { className: 'failed', label: '任务失败' })
  assert.deepEqual(sessionState(task, live('turn', 'interrupted')), { className: 'interrupted', label: '任务异常中断' })

  // 独立压缩的终态只描述那次压缩：任务保持它自己的完成状态。
  assert.deepEqual(sessionState(task, live('compaction', 'failed')), { className: 'completed', label: '任务已完成' })

  // 活动相位仍然优先；没有活动状态时用任务自身状态。
  assert.deepEqual(sessionState(task, live('compaction', 'failed', 'compacting')), { className: 'compacting', label: '正在压缩上下文' })
  assert.deepEqual(sessionState(summary({ status: 'failed' }), undefined), { className: 'failed', label: '任务失败' })
  assert.deepEqual(sessionState(summary({ status: null }), live(null, 'completed')), { className: 'idle', label: '就绪' })

  // 手动停止与异常中断的区分沿用任务自身的停止事实。
  const stopped = summary({ status: 'interrupted', manuallyStopped: true, updatedAt: startedAt })
  assert.deepEqual(sessionState(stopped, undefined), { className: 'stopped', label: '任务已停止' })
})
