import { useEffect, useRef, useState, type ReactNode } from 'react'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'
import { appStore, actionOrigin, pendingKey, useAppStore, type AppState } from '../appStore'
import { contextUsageStats, selectedSessionPhase, sessionBackgroundTasks } from '../sessionState'
import { formatTokenCount } from '../copy'
import { useDismissOnOutside } from '../interactions'
import { pop } from '../motion'
import { latestTodos, todoProgress } from '../todos'
import { ContextPanel } from './ContextPanel'
import { BackgroundTasks } from './BackgroundTasks'
import { Disclosure } from './Disclosure'
import { DiffCounts } from './EditDiff'
import { ExpandChevron } from './ExpandChevron'
import { TodoList } from './TodoList'
import type { ChangedFile } from '../protocol'
import { LoaderCircle } from 'lucide-react'

function latestCompaction(facts: NonNullable<AppState['session']>['facts'] | undefined) {
  if (!facts) return undefined
  for (let turnIndex = facts.length - 1; turnIndex >= 0; turnIndex--) {
    const items = facts[turnIndex].items
    for (let itemIndex = items.length - 1; itemIndex >= 0; itemIndex--) {
      const item = items[itemIndex]
      if (item.type === 'compaction' || item.type === 'compaction_result') return item
    }
  }
  return undefined
}

export function RunPanel({ overlay = false }: { overlay?: boolean }) {
  const state = useAppStore(['backgroundOpen', 'session', 'selectedSessionId', 'liveSessions'])
  const { backgroundOpen } = state
  const phase = selectedSessionPhase(state)
  const sessionId = state.selectedSessionId
  const cwd = state.session?.summary.cwd
  const factsCount = state.session?.facts.length ?? 0
  const lastTurnStatus = state.session?.facts.at(-1)?.status ?? ''
  const summaryRevision = sessionId ? state.liveSessions[sessionId]?.summaryRevision ?? 0 : 0
  const [changedFiles, setChangedFiles] = useState<ChangedFile[]>([])
  const openCount = useRef(0)
  const prevOpen = useRef(backgroundOpen)
  if (backgroundOpen && !prevOpen.current) openCount.current += 1
  prevOpen.current = backgroundOpen
  const openTrigger = openCount.current
  useEffect(() => {
    if (!cwd || !sessionId || factsCount === 0) {
      setChangedFiles([])
      return
    }
    let active = true
    void appStore.transport.rpc('session.changedFiles', { workspaceRoot: cwd }).then(
      files => { if (active) setChangedFiles(files) },
      () => { if (active) setChangedFiles([]) },
    )
    return () => { active = false }
  }, [cwd, sessionId, factsCount, lastTurnStatus, phase, summaryRevision, openTrigger])
  const container = useRef<HTMLDivElement>(null)
  const opener = useRef<HTMLButtonElement>(null)
  const reduced = useReducedMotion()
  useDismissOnOutside(container, backgroundOpen && overlay, () => appStore.setBackgroundOpen(false), {
    ignore: () => Boolean(document.querySelector(':modal')),
  })
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
  const usage = state.session?.runtime.contextUsage
  const { percent, tone } = contextUsageStats(usage)
  return <div ref={container} className="run-panel-anchor">
    <button ref={opener} type="button" className="status-chip run-panel-trigger" aria-label="运行状态面板"
      title={usage ? `Context ${percent}%` : 'Context'}
      aria-expanded={backgroundOpen} aria-controls="run-panel"
      onClick={() => appStore.setBackgroundOpen(!backgroundOpen)}>
      <ContextRing percent={percent} tone={tone} hasUsage={Boolean(usage)} />
    </button>
    <AnimatePresence>{backgroundOpen && <motion.aside id="run-panel" className={`float-panel${overlay ? ' is-overlay' : ''}`} aria-label="运行状态"
      initial={reduced ? false : { opacity: 0, scale: .96, y: -4 }}
      animate={{ opacity: 1, scale: 1, y: 0 }}
      exit={reduced ? { opacity: 0 } : { opacity: 0, scale: .97, y: -3 }}
      transition={reduced ? { duration: 0 } : pop}>
      <RunPanelBody changedFiles={changedFiles} />
    </motion.aside>}</AnimatePresence>
  </div>
}

function RunPanelBody({ changedFiles }: { changedFiles: ChangedFile[] }) {
  const state = useAppStore(['session', 'selectedSessionId', 'liveSessions', 'pendingActions'])
  const usage = state.session?.runtime.contextUsage
  const { percent, tone } = contextUsageStats(usage)
  const tasks = sessionBackgroundTasks(state.liveSessions, state.selectedSessionId)
  const running = tasks.filter(task => task.state.status === 'running').length
  const phase = selectedSessionPhase(state)
  const sessionId = state.selectedSessionId
  const cwd = state.session?.summary.cwd
  const compaction = latestCompaction(state.session?.facts)
  const failure = state.session?.runtime.failure
  const attempt = useRef<string | null>(null)
  const compactButton = useRef<HTMLButtonElement>(null)
  const [compactDone, setCompactDone] = useState(false)
  const [confirmingCompact, setConfirmingCompact] = useState(false)
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
  const compactDisabled = !appStore.modelAvailable() || !state.session?.facts.length
    || phase !== 'idle' || compactDone || compacting
  useEffect(() => {
    if (compactDisabled) setConfirmingCompact(false)
  }, [compactDisabled])
  useDismissOnOutside(compactButton, confirmingCompact, () => setConfirmingCompact(false), {
    capture: true,
    onEscape: event => { event.preventDefault(); setConfirmingCompact(false) },
  })
  const startCompact = () => {
    attempt.current = compaction?.id ?? ''
    void appStore.compact().then(accepted => { if (!accepted) attempt.current = null })
  }
  const onCompactClick = () => {
    if (!confirmingCompact) {
      setConfirmingCompact(true)
      return
    }
    setConfirmingCompact(false)
    startCompact()
  }
  const totalAdded = changedFiles.reduce((sum, file) => sum + file.added, 0)
  const totalDeleted = changedFiles.reduce((sum, file) => sum + file.deleted, 0)
  const todos = latestTodos(state.session?.facts)
  const { done: todosDone, total: todosTotal } = todoProgress(todos)
  return <>
      <RunSection title="Context" summary={<>
        {usage && (
          <span className="context-usage-text">
            {`${formatTokenCount(usage.used).toLowerCase()}/${formatTokenCount(usage.capacity).toLowerCase()} · `}
            <span className={`context-usage-percent${tone ? ` is-${tone}` : ''}`}>{percent}%</span>
          </span>
        )}
        <button ref={compactButton} type="button"
          className={`status-chip compact-button${compacting || compactDone ? ' is-busy' : ''}${confirmingCompact ? ' is-confirm' : ''}`}
          disabled={compactDisabled} onClick={onCompactClick}
          aria-label={confirmingCompact ? 'Confirm compact context' : `Compact context, ${percent}% used`}
          aria-busy={compacting} title={confirmingCompact ? 'Click again to compact' : 'Compact context'}>
          {compacting ? <LoaderCircle size={13} className="spinner" aria-hidden="true" />
            : compactDone ? <svg className="compact-tick" viewBox="0 0 16 16" aria-hidden="true"><path d="M3.4 8.4l3.1 3.1 6.2-6.9" /></svg>
            : confirmingCompact ? 'Confirm' : 'Compact'}
        </button>
      </>}>
        <ContextPanel />
      </RunSection>
      {changedFiles.length > 0 && cwd && (
        <section className="run-section">
          <button
            type="button"
            className="run-row run-row-action"
            onClick={() => {
              appStore.openChangedFiles(cwd, undefined, changedFiles)
            }}
          >
            <strong>Changes</strong>
            <span className="run-row-summary">
              <DiffCounts added={totalAdded} deleted={totalDeleted} />
              <ExpandChevron expanded={false} size={13} />
            </span>
          </button>
        </section>
      )}
      {todos.length > 0 && <RunSection title="Todos" initiallyOpen detail summary={`${todosDone}/${todosTotal}`}><TodoList todos={todos} /></RunSection>}
      {running > 0 && <RunSection title="Background" initiallyOpen detail summary={running}><BackgroundTasks /></RunSection>}
  </>
}

function ContextRing({ percent, tone, hasUsage }: { percent: number; tone: '' | 'warning' | 'danger'; hasUsage: boolean }) {
  return <svg className="context-ring" width="18" height="18" viewBox="0 0 20 20" aria-hidden="true">
    <circle className={`context-ring-track${hasUsage ? '' : ' is-empty'}`} cx="10" cy="10" r="7.5" />
    {hasUsage && percent > 0 && (
      <circle className={`context-ring-arc${tone ? ` is-${tone}` : ''}`} cx="10" cy="10" r="7.5" pathLength={100} strokeDasharray={`${percent} 100`} transform="rotate(-90 10 10)" />
    )}
  </svg>
}

function RunSection({ title, summary, initiallyOpen = false, detail = false, children }: { title: string; summary: ReactNode; initiallyOpen?: boolean; detail?: boolean; children: ReactNode }) {
  const [open, setOpen] = useState(initiallyOpen)
  return <section className={`run-section${detail ? ' is-detail' : ''}`}>
    <div className="run-row">
      <button type="button" className="run-row-toggle" aria-expanded={open} onClick={() => setOpen(value => !value)}>
        <strong>{title}</strong><ExpandChevron expanded={open} size={13} />
      </button>
      <span className="run-row-summary">{summary}</span>
    </div>
    <Disclosure open={open}><div className="run-section-body">{children}</div></Disclosure>
  </section>
}
