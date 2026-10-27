import { useState, type ReactNode } from 'react'
import { CircleCheck, CircleX, LoaderCircle } from 'lucide-react'
import { useAppStore } from '../appStore'
import { useSelectionGuard } from '../interactions'
import { toolDisplay } from '../toolDisplay'
import type { ToolGroup } from '../timeline'
import type { ToolStatus } from '../protocol'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

export function ToolGroupRow({ group, itemKey, sessionId, onDisclosureChange, children }: {
  group: ToolGroup; itemKey: string; sessionId?: string; onDisclosureChange: (element: HTMLElement) => void; children: ReactNode
}) {
  const [expanded, setExpanded] = useState(false)
  const guard = useSelectionGuard(onDisclosureChange)
  const { liveSessions } = useAppStore(['liveSessions'], (previous, next) =>
    previous.liveSessions[sessionId ?? '']?.backgroundTasks === next.liveSessions[sessionId ?? '']?.backgroundTasks)
  const counts: Record<ToolStatus, number> = { completed: 0, failed: 0, interrupted: 0, running: 0, pending: 0 }
  for (const row of group.rows) counts[toolDisplay(row.item, liveSessions[sessionId ?? '']?.backgroundTasks).status]++
  return <article className="timeline-item tool-group" data-item-id={itemKey}>
    <button type="button" className="activity-toggle tool-group-toggle" aria-expanded={expanded} {...guard(() => setExpanded(value => !value))}>
      {counts.completed > 0 && <CircleCheck size={13} className="ok" aria-hidden="true" />}
      <span className="step-title">{group.name}</span>
      {counts.completed > 0 && <span aria-label={`成功 ${counts.completed} 次`}>×{counts.completed}</span>}
      {counts.failed > 0 && <span className="step-label" aria-label={`失败 ${counts.failed} 次`}><CircleX size={13} className="bad" aria-hidden="true" />×{counts.failed}</span>}
      {counts.interrupted > 0 && <span className="step-label" aria-label={`中断 ${counts.interrupted} 次`}><CircleX size={13} className="bad" aria-hidden="true" />中断 ×{counts.interrupted}</span>}
      {counts.running > 0 && <span className="step-label" aria-label={`运行中 ${counts.running} 次`}><LoaderCircle size={13} className="spinner" aria-hidden="true" />×{counts.running}</span>}
      {counts.pending > 0 && <span>等待 ×{counts.pending}</span>}
      <ExpandChevron expanded={expanded} size={14} />
    </button>
    <Disclosure open={expanded}><div className="tool-group-items">{children}</div></Disclosure>
  </article>
}
