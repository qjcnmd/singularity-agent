import { backgroundTaskId, type ToolItem } from './timeline'
import type { BackgroundTask, ToolStatus } from './protocol'

/** 单条详情与分组计数共用后台任务终态，避免命令转后台后仍显示成功。 */
export function toolDisplay(item: ToolItem, tasks: BackgroundTask[] = []) {
  const id = backgroundTaskId(item)
  const task = id ? tasks.find(task => task.taskId === id) : undefined
  const status: ToolStatus = task ? task.state.status === 'running' ? 'running'
    : task.state.status === 'stopped' || task.state.status === 'interrupted' ? 'interrupted'
    : task.state.status === 'failed' || (task.state.status === 'exited' && task.state.exitCode !== 0) ? 'failed' : 'completed' : item.status
  const shown = task ? { ...item, result: {
    output: task.state.status === 'failed' ? [task.output, task.state.message].filter(Boolean).join('\n') : task.output,
    isError: status === 'failed',
    durationMs: 'durationMs' in task.state ? task.state.durationMs : undefined,
  } } : item
  return { status, shown }
}
