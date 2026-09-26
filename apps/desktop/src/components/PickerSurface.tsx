import type { ReactNode } from 'react'

/** 浮层不参与文档流，显隐只变换合成层；关闭后不保留绘制层。 */
export function PickerSurface({ open, children }: { open: boolean; children: ReactNode }) {
  return <div className={`picker-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}>
    <div>{children}</div>
  </div>
}
