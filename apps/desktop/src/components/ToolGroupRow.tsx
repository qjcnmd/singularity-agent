import { useState, type ReactNode } from 'react'
import { CircleX } from 'lucide-react'
import { useSelectionGuard } from '../interactions'
import { toolDisplay, type ToolGroup } from '../timeline'
import type { ToolStatus } from '../protocol'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { StepStatusIcon, toolTracksLiveTasks, useBackgroundTasks } from './ToolCard'

export function ToolGroupRow({ group, itemKey, sessionId, onDisclosureChange, children }: {
  group: ToolGroup; itemKey: string; sessionId?: string; onDisclosureChange: (element: HTMLElement) => void; children: ReactNode
}) {
  const [expanded, setExpanded] = useState(false)
  const guard = useSelectionGuard(onDisclosureChange)
  const tasks = useBackgroundTasks(sessionId, toolTracksLiveTasks(group.name))
  const counts: Record<ToolStatus, number> = { completed: 0, failed: 0, interrupted: 0, running: 0, pending: 0 }
  for (const row of group.rows) counts[toolDisplay(row.item, tasks).status]++
  const groupStatus: ToolStatus = counts.running > 0
    ? 'running'
    : counts.failed > 0
      ? 'failed'
      : counts.interrupted > 0
        ? 'interrupted'
        : counts.completed > 0
          ? 'completed'
          : 'pending'
  return <article className="timeline-item tool-group" data-item-id={itemKey}>
    <button type="button" className="activity-toggle tool-group-toggle" aria-expanded={expanded} {...guard(() => setExpanded(value => !value))}>
      <StepStatusIcon name={group.name} status={groupStatus} />
      <span className="step-title">{group.name}</span>
      {counts.completed > 0 && <span aria-label={`成功 ${counts.completed} 次`}>×{counts.completed}</span>}
      {counts.failed > 0 && <span className="step-label" aria-label={`失败 ${counts.failed} 次`}><CircleX size={13} className="bad" aria-hidden="true" />×{counts.failed}</span>}
      {counts.interrupted > 0 && <span className="step-label" aria-label={`中断 ${counts.interrupted} 次`}><CircleX size={13} className="bad" aria-hidden="true" />中断 ×{counts.interrupted}</span>}
      {counts.running > 0 && <span aria-label={`运行中 ${counts.running} 次`}>×{counts.running}</span>}
      {counts.pending > 0 && <span>等待 ×{counts.pending}</span>}
      <ExpandChevron expanded={expanded} size={14} />
    </button>
    <Disclosure open={expanded}><div className="tool-group-items">{children}</div></Disclosure>
  </article>
}
