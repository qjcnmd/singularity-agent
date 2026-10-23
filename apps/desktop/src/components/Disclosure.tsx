import { useLayoutEffect, useRef, useState, type ReactNode } from 'react'

/**
 * 展开/收起面板：通过 `ResizeObserver` 测量内层实际像素高度并驱动外层 `height` 过渡。
 *
 * 展开时挂载内容并过渡到实测高度；收起时先过渡到 `0px`，过渡结束后卸载内容（开启“减少动效”时立即卸载）。
 */
export function Disclosure({ open, children }: { open: boolean; children: ReactNode }) {
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
  return <div className={`content-disclosure${open ? ' is-open' : ''}`} inert={!open} aria-hidden={!open}
    style={open ? (height === null ? undefined : { height: `${height}px` }) : { height: '0px' }}
    onTransitionEnd={event => { if (event.target === event.currentTarget && event.propertyName === 'height' && !open) setMounted(false) }}>
    <div ref={inner}>{mounted ? children : null}</div>
  </div>
}
