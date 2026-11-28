import { Circle, LoaderCircle } from 'lucide-react'
import type { TodoItem } from '../todos'
import { StepCheckIcon } from './StepCheckIcon'

const statusLabels: Record<TodoItem['status'], string> = {
  completed: '已完成',
  in_progress: '进行中',
  pending: '待办',
}

export function TodoList({ todos }: { todos: TodoItem[] }) {
  if (todos.length === 0) return <p className="detail-empty todo-empty">暂无待办</p>
  return (
    <ul className="todo-list" aria-label="待办清单">
      {todos.map((item, index) => (
        <li key={index} className={`todo-item is-${item.status}`}>
          <span className="todo-icon" aria-hidden="true">
            {item.status === 'completed' ? (
              <StepCheckIcon />
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
