import { useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { useTransientFocus, useAnchoredSurface, focusableElements, navigateList } from '../interactions'

export interface MenuEntry {
  id: string
  label: string
  checked?: boolean
  disabled?: boolean
  danger?: boolean
  divider?: boolean
}

/** Shared anchored menu for workspace, session and view choices. */
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
  }}>{entries.map(entry => <button key={entry.id} type="button" role={entry.checked === undefined ? 'menuitem' : 'menuitemradio'} aria-checked={entry.checked} disabled={entry.disabled} className={`${entry.danger ? 'danger' : ''}${entry.divider ? ' menu-divider' : ''}`} onClick={() => { onClose(); onPick(entry.id) }}><span>{entry.label}</span><span aria-hidden="true">{entry.checked ? '✓' : ''}</span></button>)}</div>, document.body)
}
