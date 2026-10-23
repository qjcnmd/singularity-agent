import { useLayoutEffect, useRef, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

export function Dialog({ open, onClose, labelledBy, className = '', children }: {
  open: boolean
  onClose: () => void
  labelledBy: string
  className?: string
  children: ReactNode
}) {
  const container = useRef<HTMLDialogElement>(null)
  useLayoutEffect(() => {
    const dialog = container.current!
    if (!open) return
    dialog.showModal()
    dialog.querySelector<HTMLElement>('[data-autofocus]')?.focus({ preventScroll: true })
    return () => dialog.close()
  }, [open])
  return createPortal(
    <dialog ref={container} className={`modal ${className}`.trim()} aria-labelledby={labelledBy}
      onCancel={event => { event.preventDefault(); event.stopPropagation(); onClose() }}
      onMouseDown={event => {
        const rect = event.currentTarget.getBoundingClientRect()
        const outside = event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom
        if (event.target === event.currentTarget && outside && window.getSelection()?.isCollapsed !== false) onClose()
      }}>
      {open && children}
    </dialog>, document.body,
  )
}
