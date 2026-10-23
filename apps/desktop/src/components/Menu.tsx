import { useLayoutEffect, useRef, type RefObject } from 'react'
import { createPortal } from 'react-dom'
import { focusableElements, navigateList } from '../interactions'

interface MenuEntry {
  id: string
  label: string
  onSelect: () => void
}

/** 锚定在触发按钮旁的弹出菜单，使用原生 Popover 管理关闭与层叠。 */
export function Menu({ anchor, entries, onClose, label }: {
  anchor: RefObject<HTMLElement | null>
  entries: MenuEntry[]
  onClose: () => void
  label: string
}) {
  const container = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => {
    const menu = container.current!
    menu.showPopover({ source: anchor.current ?? undefined })
    menu.querySelector('button')?.focus({ preventScroll: true })
    return () => menu.hidePopover()
  }, [anchor])

  return createPortal(<div ref={container} popover="auto" className="context-menu" role="menu" aria-label={label} onToggle={event => { if (event.newState === 'closed') onClose() }} onKeyDown={event => {
    if (navigateList(event.key, focusableElements(event.currentTarget))) event.preventDefault()
  }}>{entries.map(entry => <button key={entry.id} type="button" role="menuitem" onClick={() => { container.current!.hidePopover(); onClose(); entry.onSelect() }}><span>{entry.label}</span></button>)}</div>, document.body)
}
