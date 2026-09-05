import type { ThreadSummary } from './protocol'

export function sessionDisplayTitle(session: ThreadSummary, siblings: ThreadSummary[] = []): string {
  const title = session.title?.trim()
  if (title !== undefined && title !== '') return title

  const untitled = siblings
    .filter((item) => item.title?.trim() === undefined || item.title.trim() === '')
    .slice()
    .sort((left, right) => left.createdAt.localeCompare(right.createdAt) || left.threadId.localeCompare(right.threadId))
  const position = untitled.findIndex((item) => item.threadId === session.threadId)
  return position >= 0 && untitled.length > 1 ? `新任务 ${position + 1}` : '新任务'
}
