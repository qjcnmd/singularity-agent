import type { ConnectionStatus, SessionPhase, TurnStatus } from './protocol'

export const uiText = {
  product: 'Singularity',
  localWorkbench: '本地工作台',
  fullLocalAccess: '完整本机权限',
  fullLocalAccessDetail: 'Agent 可以使用当前账户读取和修改本机文件、运行命令。项目只决定工作上下文，不是权限边界。',
  retry: '重试',
  close: '关闭',
  cancel: '取消',
  save: '保存',
  copy: '复制',
  copied: '已复制',
  details: '详情',
  help: '帮助',
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

export const connectionText: Record<ConnectionStatus, { title: string; detail: string }> = {
  connecting: { title: '正在连接', detail: '正在等待本地服务…' },
  ready: { title: '已连接', detail: '' },
  recovering: { title: '正在恢复', detail: '正在重连；未确认的操作不会自动重放。' },
  unauthorized: { title: '需要重新授权', detail: '请重新打开启动终端中显示的入口。' },
  unavailable: { title: '本地服务不可用', detail: '请检查启动终端，然后在这里重试。' },
}

export function formatElapsed(startedAt: string | null | undefined, now = Date.now()): string {
  if (startedAt === null || startedAt === undefined) return ''
  const started = Date.parse(startedAt)
  if (!Number.isFinite(started)) return ''
  const total = Math.max(0, Math.floor((now - started) / 1_000))
  const minutes = Math.floor(total / 60)
  const seconds = total % 60
  return minutes === 0 ? `${seconds} 秒` : `${minutes}:${seconds.toString().padStart(2, '0')}`
}
