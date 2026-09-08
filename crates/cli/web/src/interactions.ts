import { useEffect, useLayoutEffect, useRef, type MouseEvent, type PointerEvent, type RefObject } from 'react'

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
  initialFocus?: (root: HTMLElement) => HTMLElement | null,
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
    const candidates = root ? focusableElements(root) : []
    const preferred = root ? initialFocus?.(root) : null
    const focusable = preferred && candidates.includes(preferred) ? preferred : candidates.find(node => node.hasAttribute('data-autofocus')) ?? candidates[0]
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
      const nodes = focusableElements(root)
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
      const remaining = transientFocusStack.at(-1)
      const destination = returnFocus.current
      if (destination?.isConnected) {
        requestAnimationFrame(() => {
          if (destination.isConnected && transientFocusStack.at(-1) === remaining) destination.focus()
        })
      }
    }
  }, [container, open])
}

/** Only currently operable controls participate in focus navigation. */
export function focusableElements(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>('button, input, select, textarea, a[href], [tabindex]')]
    .filter(node => !node.matches(':disabled, [tabindex="-1"]') && !node.closest('[inert], [aria-hidden="true"]') && node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden')
}

export function navigateList(key: string, buttons: HTMLElement[]): boolean {
  if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(key) || buttons.length === 0) return false
  const index = buttons.indexOf(document.activeElement as HTMLElement)
  const next = key === 'Home' ? 0 : key === 'End' ? buttons.length - 1 : index < 0 ? (key === 'ArrowUp' ? buttons.length - 1 : 0) : (index + (key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length
  buttons[next]?.focus()
  return true
}

/** Shared geometry and dismissal for portal surfaces attached to a control. */
export function useAnchoredSurface(anchor: RefObject<HTMLElement | null>, container: RefObject<HTMLElement | null>, onClose: () => void) {
  const close = useRef(onClose)
  close.current = onClose
  useLayoutEffect(() => {
    const node = container.current, source = anchor.current
    if (!node || !source) return
    const position = () => {
      const rect = source.getBoundingClientRect()
      node.style.left = `${Math.max(8, Math.min(rect.left, window.innerWidth - node.offsetWidth - 8))}px`
      const below = rect.bottom + 6
      node.style.top = `${Math.max(8, Math.min(below + node.offsetHeight <= window.innerHeight - 8 ? below : rect.top - node.offsetHeight - 6, window.innerHeight - node.offsetHeight - 8))}px`
    }
    const outside = (event: globalThis.PointerEvent) => {
      if (event.target instanceof Node && !node.contains(event.target) && !source.contains(event.target)) close.current()
    }
    position()
    const observer = new ResizeObserver(position)
    observer.observe(node)
    observer.observe(source)
    document.addEventListener('pointerdown', outside)
    window.addEventListener('resize', position)
    window.addEventListener('scroll', position, true)
    return () => { observer.disconnect(); document.removeEventListener('pointerdown', outside); window.removeEventListener('resize', position); window.removeEventListener('scroll', position, true) }
  }, [anchor, container])
}
