import { useLayoutEffect, useState, type ReactNode } from 'react'

/** Keep closing content until its height transition finishes; unopened output stays lazy. */
export function Disclosure({ open, children, keepMounted = false, className = '' }: { open: boolean; children: ReactNode; keepMounted?: boolean; className?: string }) {
  const [mounted, setMounted] = useState(open)
  useLayoutEffect(() => { if (open) setMounted(true); else if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) setMounted(false) }, [open])
  return <div className={`content-disclosure${open ? ' is-open' : ''}${className ? ` ${className}` : ''}`} inert={!open} aria-hidden={!open}
    onTransitionEnd={event => { if (event.target === event.currentTarget && event.propertyName === 'grid-template-rows' && !open) setMounted(false) }}>
    <div>{mounted || keepMounted ? children : null}</div>
  </div>
}
