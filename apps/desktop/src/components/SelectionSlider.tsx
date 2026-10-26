import { useLayoutEffect, useRef, useState, type RefObject } from 'react'

/** “滑动”切换效果用的选中背景：一个垫在整列选项下面的色块，滑到带 data-selected 的那一项。
 * 色块在所有选项下层，滑过时不会盖住文字，也不会被单个选项裁切。
 * 列表里没有选中项时淡出；从无到有时直接出现在目标位置再淡入，不从别处滑过来。 */
export function SelectionSlider({ container, activeKey }: { container: RefObject<HTMLElement | null>; activeKey: string | null }) {
  const [box, setBox] = useState<{ top: number; height: number; visible: boolean; animate: boolean }>({ top: 0, height: 0, visible: false, animate: false })
  const wasVisible = useRef(false)
  useLayoutEffect(() => {
    const root = container.current
    if (!root) return
    const measure = () => {
      const target = root.querySelector<HTMLElement>(':scope > [data-selected="true"]')
      if (!target) { wasVisible.current = false; setBox(current => ({ ...current, visible: false })); return }
      const top = target.getBoundingClientRect().top - root.getBoundingClientRect().top
      setBox({ top, height: target.offsetHeight, visible: true, animate: wasVisible.current })
      wasVisible.current = true
    }
    measure()
    // 列表里有行插入、删除或展开时跟着移动
    const observer = new ResizeObserver(measure)
    observer.observe(root)
    return () => observer.disconnect()
  }, [container, activeKey])
  return <span className="selection-slider" aria-hidden="true" style={{
    transform: `translateY(${box.top}px)`, height: box.height, opacity: box.visible ? 1 : 0,
    transition: box.animate ? undefined : 'opacity 140ms',
  }} />
}
