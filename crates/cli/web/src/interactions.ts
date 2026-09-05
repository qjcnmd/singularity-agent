import { useEffect, useRef, type MouseEvent, type PointerEvent, type RefObject } from 'react'

const transientFocusStack: symbol[] = []

export function hasTextSelection(): boolean {
  const selection = window.getSelection()
  return selection !== null && !selection.isCollapsed && selection.toString() !== ''
}

export function useSelectionGuard() {
  const selectedAtPointerDown = useRef(false)
  return (activate: () => void, preventDefault = false) => ({
    onPointerDown: (_event: PointerEvent<HTMLElement>) => {
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
      if (preventDefault) event.preventDefault()
      activate()
    },
  })
}

export function useTransientFocus(
  open: boolean,
  close: () => void,
  container: RefObject<HTMLElement | null>,
) {
  const returnFocus = useRef<HTMLElement | null>(null)
  const closeRef = useRef(close)
  const identity = useRef(Symbol('transient-focus'))
  closeRef.current = close
  useEffect(() => {
    if (!open) return
    const token = identity.current
    transientFocusStack.push(token)
    returnFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null
    const root = container.current
    const focusable = root?.querySelector<HTMLElement>('[data-autofocus]')
      ?? root?.querySelector<HTMLElement>('button, input, select, textarea, [tabindex]:not([tabindex="-1"])')
    focusable?.focus()
    const onKeyDown = (event: KeyboardEvent) => {
      if (transientFocusStack.at(-1) !== token) return
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        closeRef.current()
        return
      }
      if (event.key !== 'Tab' || root === null) return
      const nodes = [...root.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])')]
      if (nodes.length === 0) return
      const first = nodes[0]
      const last = nodes[nodes.length - 1]
      if (!(document.activeElement instanceof Node) || !root.contains(document.activeElement)) {
        event.preventDefault()
        const destination = event.shiftKey ? last : first
        destination.focus()
      } else if (event.shiftKey && document.activeElement === first) {
        event.preventDefault()
        last.focus()
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault()
        first.focus()
      }
    }
    document.addEventListener('keydown', onKeyDown, true)
    return () => {
      document.removeEventListener('keydown', onKeyDown, true)
      const position = transientFocusStack.lastIndexOf(token)
      if (position >= 0) transientFocusStack.splice(position, 1)
      const destination = returnFocus.current
      if (destination?.isConnected) {
        requestAnimationFrame(() => {
          if (destination.isConnected) destination.focus()
        })
      }
    }
  }, [container, open])
}
