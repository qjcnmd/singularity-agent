import { useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { useTransientFocus, useAnchoredSurface, focusableElements, navigateList } from '../interactions'

interface MenuEntry {
  /** 列表 key；动作由条目自己的 onSelect 承担，不再由调用方按 ID 分派。 */
  id: string
  label: string
  danger?: boolean
  onSelect: () => void
}

/** workspace 与 session 命令菜单共用的锚定表面。 */
export function Menu({ anchor, entries, onClose, label }: {
  anchor: RefObject<HTMLElement | null>
  entries: MenuEntry[]
  onClose: () => void
  label: string
}) {
  const container = useRef<HTMLDivElement>(null)
  useTransientFocus(true, onClose, container)
  useAnchoredSurface(anchor, container, onClose)

  return createPortal(<div ref={container} className="context-menu" role="menu" aria-label={label} onKeyDown={event => {
    if (navigateList(event.key, focusableElements(event.currentTarget))) event.preventDefault()
  }}>{entries.map(entry => <button key={entry.id} type="button" role="menuitem" className={entry.danger ? 'danger' : ''} onClick={() => { onClose(); entry.onSelect() }}><span>{entry.label}</span></button>)}</div>, document.body)
}
