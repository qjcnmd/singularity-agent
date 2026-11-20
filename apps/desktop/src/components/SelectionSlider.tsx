import { useEffect, useRef, useState, type RefObject } from 'react'

export function SelectionSlider({ container, activeKey }: { container: RefObject<HTMLElement | null>; activeKey: string | null }) {
  const [box, setBox] = useState<{ top: number; height: number; visible: boolean; animate: boolean }>({ top: 0, height: 0, visible: false, animate: false })
  const wasVisible = useRef(false)
  useEffect(() => {
    const root = container.current
    if (!root) return
    const measure = () => {
      const target = root.querySelector<HTMLElement>(':scope > [data-selected="true"]')
      if (!target) {
        wasVisible.current = false
        setBox(current => current.visible ? { ...current, visible: false } : current)
        return
      }
      const top = target.getBoundingClientRect().top - root.getBoundingClientRect().top
      const height = target.offsetHeight
      const animate = wasVisible.current
      wasVisible.current = true
      setBox(current => current.top === top && current.height === height && current.visible && current.animate === animate
        ? current
        : { top, height, visible: true, animate })
    }
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(root)
    const rows = new MutationObserver(measure)
    rows.observe(root, { childList: true, attributes: true, attributeFilter: ['data-selected'] })
    return () => { observer.disconnect(); rows.disconnect() }
  }, [container, activeKey])
  return <span className="selection-slider" aria-hidden="true" style={{
    transform: `translateY(${box.top}px)`, height: box.height, opacity: box.visible ? 1 : 0,
    transition: box.animate ? undefined : 'opacity 140ms',
  }} />
}
