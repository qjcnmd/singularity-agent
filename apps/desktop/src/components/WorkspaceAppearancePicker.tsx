import { useRef } from 'react'
import { createPortal } from 'react-dom'
import { usePopoverFocus } from '../interactions'
import type { WorkspaceAppearance } from '../appStore'

const colors = [['灰黑', '#52525b'], ['红色', '#ef4444'], ['橙色', '#f97316'], ['黄色', '#eab308'], ['绿色', '#22a447'], ['蓝色', '#3b82f6'], ['紫色', '#8b5cf6'], ['粉色', '#ec72ad']] as const
export const defaultWorkspaceAppearance: WorkspaceAppearance = { color: colors[0][1] }

export function WorkspaceIcon({ appearance = defaultWorkspaceAppearance }: { appearance?: WorkspaceAppearance }) {
  return <span className="workspace-dot" style={{ backgroundColor: appearance.color }} aria-hidden="true" />
}

export function WorkspaceAppearancePicker({ id, open, appearance, onChange, onToggle }: {
  id: string
  open: boolean
  appearance: WorkspaceAppearance
  onChange: (appearance: WorkspaceAppearance) => void
  onToggle: (open: boolean) => void
}) {
  const root = useRef<HTMLDivElement>(null)
  usePopoverFocus(open, root)

  return createPortal(<div ref={root} id={id} popover="auto" className="workspace-appearance-picker" role="dialog" aria-label="选择项目颜色" inert={!open}
    onToggle={event => onToggle(event.newState === 'open')} onKeyDown={event => {
    if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key) || !(event.target instanceof HTMLButtonElement)) return
    const group = event.target.closest('[role="group"]')
    if (!group) return
    event.preventDefault()
    const buttons = [...group.querySelectorAll<HTMLButtonElement>('button')]
    const index = buttons.indexOf(event.target)
    const delta = event.key === 'ArrowLeft' ? -1 : 1
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + delta + buttons.length) % buttons.length
    buttons[next]?.focus()
  }}>
    <div className="workspace-color-grid" role="group" aria-label="选择颜色">{colors.map(([label, color]) => <button key={color} type="button" aria-label={label} aria-pressed={appearance.color.toLowerCase() === color} style={{ backgroundColor: color }} onClick={() => { onChange({ color }); root.current!.hidePopover() }} />)}</div>
  </div>, document.body)
}
