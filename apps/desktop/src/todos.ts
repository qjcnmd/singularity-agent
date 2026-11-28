import type { ThreadTurn } from './protocol'

type TodoStatus = 'pending' | 'in_progress' | 'completed'

export interface TodoItem {
  content: string
  status: TodoStatus
}

function isTodoStatus(value: unknown): value is TodoStatus {
  return value === 'pending' || value === 'in_progress' || value === 'completed'
}

export function parseTodos(args: unknown): TodoItem[] | null {
  if (typeof args !== 'object' || args === null || Array.isArray(args)) return null
  const raw = (args as Record<string, unknown>).todos
  if (!Array.isArray(raw)) return null
  const items: TodoItem[] = []
  for (const entry of raw) {
    if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) return null
    const { content, status } = entry as Record<string, unknown>
    if (typeof content !== 'string' || !isTodoStatus(status)) return null
    const text = content.trim()
    if (text) items.push({ content: text, status })
  }
  return items
}

export function latestTodos(facts: ThreadTurn[] | undefined): TodoItem[] {
  if (!facts) return []
  for (let turnIndex = facts.length - 1; turnIndex >= 0; turnIndex--) {
    const items = facts[turnIndex].items
    for (let itemIndex = items.length - 1; itemIndex >= 0; itemIndex--) {
      const item = items[itemIndex]
      if (item.type !== 'tool' || item.name !== 'todo' || item.status !== 'completed' || item.result?.isError) continue
      const parsed = parseTodos(item.args)
      if (parsed !== null) return parsed
    }
  }
  return []
}

export function todoProgress(todos: TodoItem[]): { done: number; total: number } {
  let done = 0
  for (const item of todos) if (item.status === 'completed') done++
  return { done, total: todos.length }
}

export function todoSummary(todos: TodoItem[]): string {
  if (todos.length === 0) return '已清空'
  const { done, total } = todoProgress(todos)
  const active = todos.find(item => item.status === 'in_progress')
  return active ? `${done}/${total} · ${active.content}` : `${done}/${total}`
}
