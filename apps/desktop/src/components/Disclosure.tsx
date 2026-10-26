import { useLayoutEffect, useState, type ReactNode } from 'react'

/** 用 CSS 在 0 和 auto 高度间过渡；已展开的外层直接跟随内部折叠项，不再叠加一轮动画。 */
export function Disclosure({ open, children }: { open: boolean; children: ReactNode }) {
  const [mounted, setMounted] = useState(open)
  useLayoutEffect(() => {
    if (open) setMounted(true)
    else if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) setMounted(false)
  }, [open])
  return <div className={`content-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}
    onTransitionEnd={event => { if (event.target === event.currentTarget && event.propertyName === 'height' && !open) setMounted(false) }}>
    <div>{mounted ? children : null}</div>
  </div>
}
