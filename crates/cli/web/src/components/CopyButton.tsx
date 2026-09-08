import { useEffect, useRef, useState } from 'react'
import { useSelectionGuard } from '../interactions'

export function CopyButton({ text, label = '复制' }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false)
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined)
  const revision = useRef(0)
  const guard = useSelectionGuard()
  useEffect(() => {
    setCopied(false)
    return () => { revision.current++; clearTimeout(timer.current) }
  }, [text])
  const copy = async () => {
    const current = ++revision.current
    clearTimeout(timer.current)
    setCopied(false)
    try {
      await navigator.clipboard.writeText(text)
      if (current !== revision.current) return
      setCopied(true)
      timer.current = setTimeout(() => setCopied(false), 1200)
    } catch { /* A failed copy must not show success. */ }
  }
  return <button type="button" className="quiet-button" {...guard(() => { void copy() })}>{copied ? '已复制' : label}</button>
}
