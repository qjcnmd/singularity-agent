import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { appStore, actionOrigin, pendingKey, useAppStore } from '../appStore'
import { selectedSessionPhase } from '../sessionState'
import { formatTokenCount } from '../copy'
import { pop } from '../motion'
import { ContextPanel } from './ContextPanel'
import { BackgroundTasks } from './BackgroundTasks'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import { Shrink } from 'lucide-react'

export function RunPanel() {
  const state = useAppStore(['session', 'selectedSessionId', 'backgroundOpen', 'liveSessions', 'connection', 'pendingActions'])
  const opener = useRef<HTMLButtonElement>(null)
  const reduced = useReducedMotion()
  const usage = state.session?.runtime.contextUsage
  const percent = usage ? Math.min(100, Math.round(usage.used / usage.capacity * 100)) : 0
  const tasks = state.selectedSessionId ? state.liveSessions[state.selectedSessionId]?.backgroundTasks ?? [] : []
  const running = tasks.filter(task => task.state.status === 'running').length
  const phase = selectedSessionPhase(state)
  const compactDisabled = !appStore.modelAvailable() || !state.session?.facts.length || state.connection !== 'ready'
    || phase !== 'idle' || state.pendingActions.has(pendingKey('session.compact', actionOrigin.session(state.selectedSessionId)))
  useEffect(() => {
    const close = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented || !appStore.getSnapshot().backgroundOpen) return
      if (document.querySelector(':modal, :popover-open')) return
      appStore.setBackgroundOpen(false)
      opener.current?.focus()
    }
    window.addEventListener('keydown', close)
    return () => window.removeEventListener('keydown', close)
  }, [])
  return <div className="topbar-actions">
    <button ref={opener} type="button" className={`status-chip${state.backgroundOpen ? ' is-open' : ''}`}
      aria-expanded={state.backgroundOpen} aria-controls="run-panel"
      aria-label={`运行状态：上下文 ${percent}%${running ? `，${running} 个后台命令运行中` : ''}`}
      onClick={() => appStore.setBackgroundOpen(!state.backgroundOpen)}>
      <svg width="16" height="16" viewBox="0 0 20 20" aria-hidden="true">
        <circle cx="10" cy="10" r="7.5" fill="none" stroke="rgb(0 0 0 / 10%)" strokeWidth="2.4" />
        <circle cx="10" cy="10" r="7.5" fill="none" stroke="var(--accent)" strokeWidth="2.4" pathLength={100} strokeDasharray={`${percent} 100`} strokeLinecap="round" transform="rotate(-90 10 10)" />
      </svg><span>{percent}%</span>{running > 0 && <span className="status-running"><i />{running}</span>}
    </button>
    <AnimatePresence>{state.backgroundOpen && <motion.aside id="run-panel" className="float-panel" aria-label="运行状态"
      initial={{ opacity: 0, scale: .94, y: -6 }} animate={{ opacity: 1, scale: 1, y: 0 }} exit={{ opacity: 0, scale: .96, y: -4 }} transition={reduced ? { duration: 0 } : pop}>
      <Card title="上下文" summary={<>{percent}%<span className="mini-bar"><i style={{ width: `${percent}%` }} /></span></>}>
        <div className="float-usage">
          {usage && <span className="float-note">已用 {formatTokenCount(usage.used).toLowerCase()} / {formatTokenCount(usage.capacity).toLowerCase()} token</span>}
          <button type="button" className="compact-button" disabled={compactDisabled} onClick={() => void appStore.compact()}><Shrink size={13} strokeWidth={1.8} aria-hidden="true" />{phase === 'compacting' ? '压缩中…' : '压缩上下文'}</button>
        </div><ContextPanel />
      </Card>
      {running > 0 && <Card title="后台命令" initiallyOpen summary={<span className="running-summary"><i />{running} 个运行中</span>}><BackgroundTasks /></Card>}
    </motion.aside>}</AnimatePresence>
  </div>
}

function Card({ title, summary, initiallyOpen = false, children }: { title: string; summary: ReactNode; initiallyOpen?: boolean; children: ReactNode }) {
  const [open, setOpen] = useState(initiallyOpen)
  return <section className="float-card">
    <button type="button" className="float-card-head" aria-expanded={open} onClick={() => setOpen(value => !value)}>
      <strong>{title}</strong><ExpandChevron expanded={open} size={13} /><span className="float-card-summary">{summary}</span>
    </button><Disclosure open={open}><div className="float-card-body">{children}</div></Disclosure>
  </section>
}
