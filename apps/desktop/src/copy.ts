import type { SessionPhase, ToolStatus, TurnStatus, Workspace } from './protocol'

export function workspaceName(workspace: Workspace): string {
  return workspace.root.slice(workspace.root.lastIndexOf('/') + 1) || workspace.root
}

export const factStatusText: Record<ToolStatus | TurnStatus, string> = {
  pending: '等待执行',
  running: '进行中',
  completed: '已完成',
  failed: '失败',
  interrupted: '已停止',
}

export const phaseText: Record<SessionPhase, string> = {
  idle: '就绪',
  running: '正在运行',
  compacting: '正在压缩上下文',
  stopping: '正在停止',
}

export const turnStatusText: Record<TurnStatus, string> = {
  running: '正在运行',
  completed: '任务已完成',
  failed: '任务失败',
  interrupted: '任务已停止',
}

export function interruptionText(manuallyStopped: boolean): string {
  return manuallyStopped ? factStatusText.interrupted : '任务异常中断'
}

export const compactionText = {
  running: '正在压缩上下文…',
  completed: '上下文已压缩',
  failed: '上下文压缩失败',
  unfinished: '压缩未完成',
  unchanged: '当前内容不足，无需压缩',
} as const

export function formatTokenCount(value: number): string {
  if (value < 1000) return String(value)
  if (value < 1_000_000) return `${Number((value / 1000).toFixed(1))}K`
  return `${Number((value / 1_000_000).toFixed(2))}M`
}
