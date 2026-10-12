import { useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { motion, useIsPresent, useReducedMotion } from 'motion/react'
import { useTransientFocus, useAnchoredSurface } from '../interactions'
import type { WorkspaceAppearance } from '../appStore'
import { disclosureTransition } from '../motion'

const colors = [['灰黑', '#52525b'], ['红色', '#ef4444'], ['橙色', '#f97316'], ['黄色', '#eab308'], ['绿色', '#22a447'], ['蓝色', '#3b82f6'], ['紫色', '#8b5cf6'], ['粉色', '#ec72ad']] as const
export const defaultWorkspaceAppearance: WorkspaceAppearance = { color: colors[0][1] }

export function WorkspaceIcon({ appearance = defaultWorkspaceAppearance }: { appearance?: WorkspaceAppearance }) {
  return <span className="workspace-dot" style={{ backgroundColor: appearance.color }} aria-hidden="true" />
}

export function WorkspaceAppearancePicker({ anchor, appearance, onChange, onClose }: {
  anchor: RefObject<HTMLElement | null>
  appearance: WorkspaceAppearance
  onChange: (appearance: WorkspaceAppearance) => void
  onClose: () => void
}) {
  const root = useRef<HTMLDivElement>(null)
  const present = useIsPresent()
  const reducedMotion = useReducedMotion()
  useTransientFocus(present, onClose, root)
  useAnchoredSurface(anchor, root, onClose)

  return createPortal(<motion.div ref={root} className="workspace-appearance-picker" role="dialog" aria-label="选择项目颜色" inert={!present}
    initial={{ opacity: 0, transform: reducedMotion ? 'none' : 'translateY(-4px) scale(.96)' }}
    animate={{ opacity: 1, transform: 'translateY(0) scale(1)' }}
    exit={{ opacity: 0, transform: reducedMotion ? 'none' : 'translateY(-4px) scale(.96)' }}
    transition={disclosureTransition(present, reducedMotion)} onKeyDown={event => {
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
    <div className="workspace-color-grid" role="group" aria-label="选择颜色">{colors.map(([label, color]) => <button key={color} type="button" aria-label={label} aria-pressed={appearance.color.toLowerCase() === color} style={{ backgroundColor: color }} onClick={() => { onChange({ color }); onClose() }} />)}</div>
  </motion.div>, document.body)
}
