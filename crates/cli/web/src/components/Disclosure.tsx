import { useLayoutEffect, useRef, useState, type ReactNode } from 'react'

/**
 * 高度披露：内容自行决定高度，容器按量到的像素过渡 `height`。
 *
 * 高度由内层量出，而不是让容器按轨道比例推算——容器在过渡中才会与实际内容一致，
 * 文档流内的展开不会把下方内容提前推开。内层始终挂载着量（`overflow: hidden`
 * 只裁剪绘制，不改布局高度），因此收起状态下也能读到真实高度。
 *
 * 在高度过渡结束前保留正在关闭的内容；未展开的输出保持惰性。
 */
export function Disclosure({ open, children, keepMounted = false, className = '' }: { open: boolean; children: ReactNode; keepMounted?: boolean; className?: string }) {
  const [mounted, setMounted] = useState(open)
  const [height, setHeight] = useState<number | null>(null)
  const inner = useRef<HTMLDivElement>(null)
  useLayoutEffect(() => { if (open) setMounted(true); else if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) setMounted(false) }, [open])
  // 内容变化（列表增删、文本换行、字体就绪）时重新量，展开态下随之平滑过渡到新高度。
  useLayoutEffect(() => {
    const element = inner.current
    if (!element) return
    const measure = () => setHeight(element.getBoundingClientRect().height)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(element)
    return () => observer.disconnect()
  }, [mounted])
  return <div className={`content-disclosure${open ? ' is-open' : ''}${className ? ` ${className}` : ''}`} inert={!open} aria-hidden={!open}
    style={open ? (height === null ? undefined : { height: `${height}px` }) : { height: '0px' }}
    onTransitionEnd={event => { if (event.target === event.currentTarget && event.propertyName === 'height' && !open) setMounted(false) }}>
    <div ref={inner}>{mounted || keepMounted ? children : null}</div>
  </div>
}
