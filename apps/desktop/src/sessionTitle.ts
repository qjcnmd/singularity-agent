import type { ThreadSummary } from './protocol'

/** 同一项目下所有任务的显示标题。
 *
 * 未命名任务按创建时间编号（“新任务 1”“新任务 2”……），页头和侧栏用同一套编号；
 * 侧栏的隐藏、折叠和只显示前几项只影响哪些行出现，不影响编号。
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
