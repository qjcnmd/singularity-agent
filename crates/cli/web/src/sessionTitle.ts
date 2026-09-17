import type { ThreadSummary } from './protocol'

/** 一组任务（同一项目的完整任务列表）的显示标题派生。
 *
 * 未命名任务的序号只由这一组决定：页头与侧栏按同一份派生结果取名，可见性过滤
 * （隐藏空白任务、折叠、只显示前几项）只决定哪些行出现，不参与命名。
 */
export function sessionTitles(sessions: ThreadSummary[]): (session: ThreadSummary) => string {
  const untitled = sessions
    .filter((item) => (item.title?.trim() ?? '') === '')
    .sort((left, right) => left.createdAt.localeCompare(right.createdAt) || left.threadId.localeCompare(right.threadId))
  const order = new Map(untitled.map((item, index) => [item.threadId, index]))
  return (session) => {
    const title = session.title?.trim()
    if (title !== undefined && title !== '') return title
    const index = order.get(session.threadId)
    return index !== undefined && untitled.length > 1 ? `新任务 ${index + 1}` : '新任务'
  }
}
