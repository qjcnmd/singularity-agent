import type { ThreadSummary } from './protocol'

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
