import type { Ref } from 'react'

export function SidebarToggle({ side, expanded, controls, onClick, buttonRef, className = '' }: {
  side: 'left' | 'right'
  expanded: boolean
  controls: string
  onClick: () => void
  buttonRef?: Ref<HTMLButtonElement>
  className?: string
}) {
  const label = `${expanded ? '关闭' : '打开'}${side === 'left' ? '左' : '右'}侧栏`
  return <button ref={buttonRef} type="button" className={`header-button sidebar-toggle ${className}`} aria-label={label} title={label} aria-expanded={expanded} aria-controls={controls} onClick={onClick}>
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true"><rect x="3" y="4" width="18" height="16" rx="3" /><path d={side === 'left' ? 'M9 4v16' : 'M15 4v16'} /></svg>
  </button>
}
