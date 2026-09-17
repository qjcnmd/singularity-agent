import type { FactStatus } from './execution'
import type { SessionPhase, TurnStatus } from './protocol'

/// 执行事实状态的统一词表：时间线与轨迹共用一份。展示差异（例如时间线不显示
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
