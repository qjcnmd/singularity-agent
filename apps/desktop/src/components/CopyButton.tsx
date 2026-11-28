import { useEffect, useRef, useState } from 'react'
import { Check, Copy, CircleAlert } from 'lucide-react'

export function CopyButton({ text, label = '复制', iconOnly = false }: { text: string; label?: string; iconOnly?: boolean }) {
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copied' | 'error'>('idle')
  const stateRef = useRef<{ revision: number; timer: ReturnType<typeof setTimeout> | undefined }>({
    revision: 0,
    timer: undefined,
  })
  useEffect(() => {
    const state = stateRef.current
    setCopyStatus('idle')
    return () => { state.revision++; clearTimeout(state.timer) }
  }, [text])
  const copy = async () => {
    const current = ++stateRef.current.revision
    clearTimeout(stateRef.current.timer)
    setCopyStatus('idle')
    try {
      await navigator.clipboard.writeText(text)
      if (current !== stateRef.current.revision) return
      setCopyStatus('copied')
      stateRef.current.timer = setTimeout(() => setCopyStatus('idle'), 1200)
    } catch {
      if (current === stateRef.current.revision) setCopyStatus('error')
    }
  }
  return <button type="button" className="quiet-button" aria-live="polite"
    aria-label={copyStatus === 'error' ? `${label}失败，点击重试` : iconOnly ? copyStatus === 'copied' ? '已复制' : label : undefined}
    onClick={() => { void copy() }}>
    {iconOnly ? copyStatus === 'copied' ? <Check size={14} aria-hidden="true" /> : copyStatus === 'error' ? <CircleAlert size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />
      : copyStatus === 'copied' ? '已复制' : copyStatus === 'error' ? '复制失败，重试' : label}
  </button>
}
