import { useCallback, useRef, type ReactNode } from 'react'
import { useTransientFocus } from '../interactions'

export function Dialog({
  open,
  onClose,
  labelledBy,
  className = '',
  children,
}: {
  open: boolean
  onClose: () => void
  labelledBy: string
  className?: string
  children: ReactNode
}) {
  const container = useRef<HTMLElement>(null)
  const close = useCallback(() => onClose(), [onClose])
  useTransientFocus(open, close, container)
  if (!open) return null
  return (
    <div
      className="modal-backdrop"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget && window.getSelection()?.isCollapsed !== false) close()
      }}
    >
      <section ref={container} className={`modal ${className}`.trim()} role="dialog" aria-modal="true" aria-labelledby={labelledBy}>
        {children}
      </section>
    </div>
  )
}
