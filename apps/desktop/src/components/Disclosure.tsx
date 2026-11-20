import { useEffect, useLayoutEffect, useState, type ReactNode } from 'react'

export function Disclosure({ open, children }: { open: boolean; children: ReactNode }) {
  const [mounted, setMounted] = useState(open)
  useLayoutEffect(() => {
    if (open) setMounted(true)
    else if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) setMounted(false)
  }, [open])
  useEffect(() => {
    if (open || !mounted) return
    const timer = window.setTimeout(() => setMounted(false), 450)
    return () => window.clearTimeout(timer)
  }, [open, mounted])
  return <div className={`content-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}
    onTransitionEnd={event => { if (event.target === event.currentTarget && event.propertyName === 'height' && !open) setMounted(false) }}>
    <div>{mounted ? children : null}</div>
  </div>
}
