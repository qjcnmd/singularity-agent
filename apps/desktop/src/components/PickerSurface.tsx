import type { ReactNode } from 'react'

/** 下拉浮层容器，管理展开过渡动画与无障碍属性。 */
export function PickerSurface({ open, children }: { open: boolean; children: ReactNode }) {
  return <div className={`picker-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}>
    <div>{children}</div>
  </div>
}
