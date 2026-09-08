import type { SessionPhase, TurnStatus } from './protocol'

export const uiText = {
  product: 'Singularity',
  retry: '重试',
  close: '关闭',
  cancel: '取消',
  save: '保存',
  copied: '已复制',
  models: '模型',
} as const

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
