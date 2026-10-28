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
import { Ellipsis, LoaderCircle } from 'lucide-react'

export function RunPanel() {
  const state = useAppStore(['session', 'selectedSessionId', 'backgroundOpen', 'liveSessions', 'connection', 'pendingActions'])
  const opener = useRef<HTMLButtonElement>(null)
  const reduced = useReducedMotion()
  const usage = state.session?.runtime.contextUsage
  const percent = usage ? Math.min(100, Math.round(usage.used / usage.capacity * 100)) : 0
  const tasks = state.selectedSessionId ? state.liveSessions[state.selectedSessionId]?.backgroundTasks ?? [] : []
  const running = tasks.filter(task => task.state.status === 'running').length
  const phase = selectedSessionPhase(state)
  const sessionId = state.selectedSessionId
  const compaction = state.session?.facts.flatMap(turn => turn.items)
    .findLast(item => item.type === 'compaction' || item.type === 'compaction_result')
  const failure = state.session?.runtime.failure
  const attempt = useRef<string | null>(null)
  const [compactDone, setCompactDone] = useState(false)
  // 结果条目先于空闲状态到达；按本次条目的终态反馈，不猜测事件间隔。
  useEffect(() => {
    if (attempt.current === null) return
    if (failure?.source === 'compaction') attempt.current = null
    else if (compaction && compaction.id !== attempt.current
      && (compaction.type === 'compaction' || compaction.status !== 'running')) {
      attempt.current = null
      setCompactDone(compaction.type === 'compaction')
    }
  }, [compaction, failure])
  useEffect(() => {
    if (!compactDone) return
    const timer = window.setTimeout(() => setCompactDone(false), 1200)
    return () => window.clearTimeout(timer)
  }, [compactDone])
  const compacting = phase === 'compacting' || state.pendingActions.has(pendingKey('session.compact', actionOrigin.session(sessionId)))
  const compactDisabled = !appStore.modelAvailable() || !state.session?.facts.length || state.connection !== 'ready'
    || phase !== 'idle' || compactDone || state.pendingActions.has(pendingKey('session.compact', actionOrigin.session(state.selectedSessionId)))
  const startCompact = () => {
    attempt.current = compaction?.id ?? ''
    void appStore.compact().then(accepted => { if (!accepted) attempt.current = null })
  }
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
    <button ref={opener} type="button" className="status-chip" aria-label="运行状态面板"
      aria-expanded={state.backgroundOpen} aria-controls="run-panel"
      onClick={() => appStore.setBackgroundOpen(!state.backgroundOpen)}><Ellipsis size={18} /></button>
    <AnimatePresence>{state.backgroundOpen && <motion.aside id="run-panel" className="float-panel" aria-label="运行状态"
      initial={{ opacity: 0, scale: .94, y: -6 }} animate={{ opacity: 1, scale: 1, y: 0 }} exit={{ opacity: 0, scale: .96, y: -4 }} transition={reduced ? { duration: 0 } : pop}>
      <Card title="上下文" summary={<span className="compact-slot" style={{ width: `calc(${String(percent).length + 1}ch + 37px)` }}>
        <button type="button" className={`status-chip compact-capsule${compacting || compactDone ? ' is-circle' : ''}`} disabled={compactDisabled} onClick={startCompact}
          aria-label={`压缩上下文，当前 ${percent}%`} aria-busy={compacting} title="压缩上下文">
          <span className="compact-icon" aria-hidden="true">
            {compacting ? <LoaderCircle size={16} className="spinner" />
              : compactDone ? <svg className="compact-tick" viewBox="0 0 16 16"><path d="M3.4 8.4l3.1 3.1 6.2-6.9" /></svg>
              : <ContextRing percent={percent} />}
          </span>
          <span className="compact-percent">{percent}%</span>
        </button>
      </span>}>
        {usage && <div className="float-usage"><span className="float-note">已用 {formatTokenCount(usage.used).toLowerCase()} / {formatTokenCount(usage.capacity).toLowerCase()} token</span></div>}
        <ContextPanel />
      </Card>
      {running > 0 && <Card title="后台命令" initiallyOpen summary={running}><BackgroundTasks /></Card>}
    </motion.aside>}</AnimatePresence>
  </div>
}

function ContextRing({ percent }: { percent: number }) {
  return <svg width="16" height="16" viewBox="0 0 20 20" aria-hidden="true">
    <circle cx="10" cy="10" r="7.5" fill="none" stroke="var(--line)" strokeWidth="2.4" />
    <circle cx="10" cy="10" r="7.5" fill="none" stroke="var(--accent)" strokeWidth="2.4" pathLength={100} strokeDasharray={`${percent} 100`} strokeLinecap="round" transform="rotate(-90 10 10)" />
  </svg>
}

function Card({ title, summary, initiallyOpen = false, children }: { title: string; summary: ReactNode; initiallyOpen?: boolean; children: ReactNode }) {
  const [open, setOpen] = useState(initiallyOpen)
  return <section className="float-card">
    <div className="float-card-head">
      <button type="button" className="float-card-toggle" aria-expanded={open} onClick={() => setOpen(value => !value)}>
        <strong>{title}</strong><ExpandChevron expanded={open} size={13} />
      </button>
      <span className="float-card-summary">{summary}</span>
    </div><Disclosure open={open}><div className="float-card-body">{children}</div></Disclosure>
  </section>
}
