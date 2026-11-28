import { useId, useLayoutEffect, useRef, type ReactNode } from 'react'
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

export function ConfirmDialog({
  open,
  onClose,
  onConfirm,
  title,
  message,
  confirmLabel,
  cancelLabel = '取消',
  danger = false,
  pending = false,
  error,
}: {
  open: boolean
  onClose: () => void
  onConfirm: () => void
  title: string
  message: ReactNode
  confirmLabel: string
  cancelLabel?: string
  danger?: boolean
  pending?: boolean
  error?: string | null
}) {
  const titleId = useId()
  const handleClose = () => { if (!pending) onClose() }
  return (
    <Dialog open={open} onClose={handleClose} labelledBy={titleId} className="confirm-modal">
      <header className="modal-header"><h2 id={titleId}>{title}</h2></header>
      <div className="confirm-body">
        <p>{message}</p>
        {error && <p role="alert" className="form-error">{error}</p>}
        <footer>
          <button type="button" className="secondary-button" data-autofocus disabled={pending} onClick={handleClose}>{cancelLabel}</button>
          <button type="button" className={danger ? 'danger-button' : 'primary-button'} disabled={pending} onClick={onConfirm}>{confirmLabel}</button>
        </footer>
      </div>
    </Dialog>
  )
}
