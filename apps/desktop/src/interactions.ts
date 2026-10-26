import { useEffect, useLayoutEffect, useRef, type MouseEvent, type RefObject } from 'react'

function hasTextSelection(): boolean {
  const selection = window.getSelection()
  return selection !== null && !selection.isCollapsed && selection.toString() !== ''
}

/**
 * 拖选不触发控件：按下或释放时存在文字选区就不激活。
 * 未拦截时由调用方自己决定行为；原生链接等控件不需要 activate。
 */
export function useSelectionGuard() {
  const selectedAtPointerDown = useRef(false)
  return (activate?: () => void) => ({
    onPointerDown: () => {
      selectedAtPointerDown.current = hasTextSelection()
    },
    onClick: (event: MouseEvent<HTMLElement>) => {
      if (hasTextSelection() || selectedAtPointerDown.current) {
        event.preventDefault()
        event.stopPropagation()
        selectedAtPointerDown.current = false
        return
      }
      selectedAtPointerDown.current = false
      activate?.()
    },
  })
}

/** 原生按钮打开 Popover 后，设置各选择器的首次焦点。 */
export function usePopoverFocus(
  open: boolean,
  container: RefObject<HTMLElement | null>,
  initialFocus = 'button:not(:disabled)',
) {
  useLayoutEffect(() => {
    if (open) container.current!.querySelector<HTMLElement>(initialFocus)?.focus({ preventScroll: true })
  }, [open, container, initialFocus])
}

/** 筛选容器内当前可见、未禁用且未被 `inert`/`aria-hidden` 隐藏的可聚焦元素。 */
export function focusableElements(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>('button, input, select, textarea, summary, a[href], [tabindex]')]
    .filter(node => !node.matches(':disabled, [tabindex="-1"]') && !node.closest('[inert], [aria-hidden="true"]') && node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden')
}

export function navigateList(key: string, buttons: HTMLElement[]): boolean {
  if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(key) || buttons.length === 0) return false
  const index = buttons.indexOf(document.activeElement as HTMLElement)
  let next = index
  if (key === 'Home') next = 0
  else if (key === 'End') next = buttons.length - 1
  else if (index < 0) next = key === 'ArrowDown' ? 0 : buttons.length - 1
  else next = (index + (key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length
  buttons[next].focus()
  return true
}

export function useDismissOnOutside(
  container: RefObject<HTMLElement | null>, active: boolean, onClose: () => void,
) {
  const close = useRef(onClose)
  close.current = onClose
  useEffect(() => {
    if (!active) return
    const outside = (event: Event) => {
      if (event.target instanceof Node && !container.current?.contains(event.target)) close.current()
    }
    document.addEventListener('pointerdown', outside)
    return () => {
      document.removeEventListener('pointerdown', outside)
    }
  }, [container, active])
}
