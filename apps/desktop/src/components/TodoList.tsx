import { Circle, LoaderCircle } from 'lucide-react'
import type { ThreadTurn } from '../protocol'

export type TodoStatus = 'pending' | 'in_progress' | 'completed'

export interface TodoItem {
  content: string
  status: TodoStatus
}

const statusLabels: Record<TodoStatus, string> = {
  completed: '已完成',
  in_progress: '进行中',
  pending: '待办',
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

export function TodoList({ todos }: { todos: TodoItem[] }) {
  if (todos.length === 0) return <p className="detail-empty todo-empty">暂无待办</p>
  return (
    <ul className="todo-list" aria-label="待办清单">
      {todos.map((item, index) => (
        <li key={index} className={`todo-item is-${item.status}`}>
          <span className="todo-icon" aria-hidden="true">
            {item.status === 'completed' ? (
              <svg className="step-tick" width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
                <circle className="step-tick-ring" cx="8" cy="8" r="6.25" />
                <path className="step-tick-check" d="M5.1 8.2l2 2 3.8-4.2" />
              </svg>
            ) : item.status === 'in_progress'
              ? <LoaderCircle size={14} className="spinner step-spinner" aria-hidden="true" />
              : <Circle size={13} strokeWidth={1.75} aria-hidden="true" />}
          </span>
          <span className="sr-only">{statusLabels[item.status]}：</span>
          <span className="todo-text">{item.content}</span>
        </li>
      ))}
    </ul>
  )
}
