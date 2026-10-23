import type { ReactNode } from 'react'

/** 下拉浮层容器，控制展开动效与隐藏状态下的 `inert`/`aria-hidden`。 */
export function PickerSurface({ open, children }: { open: boolean; children: ReactNode }) {
  return <div className={`picker-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}>
    <div>{children}</div>
  </div>
}
