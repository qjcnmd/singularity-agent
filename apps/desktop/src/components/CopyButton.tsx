import { useEffect, useRef, useState } from 'react'
import { useSelectionGuard } from '../interactions'

export function CopyButton({ text, label = '复制' }: { text: string; label?: string }) {
  const [copyStatus, setCopyStatus] = useState<'idle' | 'copied' | 'error'>('idle')
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const revision = useRef(0)
  const guard = useSelectionGuard()
  useEffect(() => {
    setCopyStatus('idle')
    return () => { revision.current++; clearTimeout(timer.current) }
  }, [text])
  const copy = async () => {
    const current = ++revision.current
    clearTimeout(timer.current)
    setCopyStatus('idle')
    try {
      await navigator.clipboard.writeText(text)
      if (current !== revision.current) return
      setCopyStatus('copied')
      timer.current = setTimeout(() => setCopyStatus('idle'), 1200)
    } catch {
      if (current === revision.current) setCopyStatus('error')
    }
  }
  return <button type="button" className="quiet-button" aria-live="polite"
    aria-label={copyStatus === 'error' ? `${label}失败，点击重试` : undefined}
    title={copyStatus === 'error' ? '请重试或手动选择文字复制。' : undefined}
    {...guard(() => { void copy() })}>
    {copyStatus === 'copied' ? '已复制' : copyStatus === 'error' ? '复制失败，重试' : label}
  </button>
}
