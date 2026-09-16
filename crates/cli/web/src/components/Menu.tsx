import { useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { useTransientFocus, useAnchoredSurface, focusableElements, navigateList } from '../interactions'

export interface MenuEntry {
  id: string
  label: string
  danger?: boolean
}

/** workspace 与 session 命令菜单共用的锚定表面。 */
export function Menu({ anchor, entries, onPick, onClose, label }: {
  anchor: RefObject<HTMLElement | null>
  entries: MenuEntry[]
  onPick: (id: string) => void
  onClose: () => void
  label: string
}) {
  const container = useRef<HTMLDivElement>(null)
  useTransientFocus(true, onClose, container)
  useAnchoredSurface(anchor, container, onClose)

  return createPortal(<div ref={container} className="context-menu" role="menu" aria-label={label} onKeyDown={event => {
    if (navigateList(event.key, focusableElements(event.currentTarget))) event.preventDefault()
  }}>{entries.map(entry => <button key={entry.id} type="button" role="menuitem" className={entry.danger ? 'danger' : ''} onClick={() => { onClose(); onPick(entry.id) }}><span>{entry.label}</span></button>)}</div>, document.body)
}
