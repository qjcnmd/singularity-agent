import type { ReactNode } from 'react'

export function PickerSurface({ open, children }: { open: boolean; children: ReactNode }) {
  return <div className={`picker-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}>
    <div>{children}</div>
  </div>
}
