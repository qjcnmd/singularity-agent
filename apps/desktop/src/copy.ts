import type { FactStatus } from './execution'
import type { SessionPhase, TurnStatus, Workspace } from './protocol'

/** 项目路径由后端统一为正斜杠；盘符根目录直接显示完整路径。 */
export function workspaceName(workspace: Workspace): string {
  return workspace.root.slice(workspace.root.lastIndexOf('/') + 1) || workspace.root
}

/// 执行事实状态的统一词表：会话显示共用一份。展示差异（例如时间线不显示
/// stable 标签）由各组件自己决定，不通过复制整张映射实现。
export const factStatusText: Record<FactStatus, string> = {
  stable: '已记录',
  running: '进行中',
  ok: '已完成',
  error: '失败',
  cancelled: '已停止',
}

export const phaseText: Record<SessionPhase, string> = {
  idle: '就绪',
  reserved: '正在启动',
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

/** 用户停止与异常中断共用同一判断，正文和侧栏不能各自推断来源。 */
export function interruptionText(manuallyStopped: boolean): string {
  return manuallyStopped ? factStatusText.cancelled : '任务异常中断'
}

export const compactionTitle = '上下文压缩'
export const compactionText = {
  running: '正在压缩上下文…',
  completed: '上下文已压缩',
  failed: '上下文压缩失败',
  unfinished: '压缩未完成',
  unchanged: '没有可压缩的内容',
} as const

/** Token 数量的紧凑展示；编辑输入保留精确容量。 */
export function formatTokenCount(value: number): string {
  if (value < 1000) return String(value)
  if (value < 1_000_000) return `${Number((value / 1000).toFixed(1))}K`
  return `${Number((value / 1_000_000).toFixed(2))}M`
}
