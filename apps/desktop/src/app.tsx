import { actionToastTransition } from './motion'
import { focusableElements } from './interactions'
import { SidebarToggle } from './components/SidebarToggle'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Settings } from './components/Settings'
import { Sidebar } from './components/Sidebar'
import { Trajectory } from './components/Trajectory'
import { hasInlineActionError, useAppStore, appStore } from './appStore'
import { buildTimeline } from './timeline'
import { sessionTitles } from './sessionTitle'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

export function App() {
  const state = useAppStore(['theme', 'messageFontSize', 'selectedSessionId', 'selectedWorkspaceId', 'bootstrap', 'sidebarCollapsed', 'sidebarWidth', 'trajectoryWidth', 'trajectoryOpen', 'settingsOpen', 'actionErrors', 'pendingActions'])
  useLayoutEffect(() => { document.documentElement.dataset.theme = state.theme }, [state.theme])
  useLayoutEffect(() => { document.documentElement.style.setProperty('--message-font-size', `${state.messageFontSize}px`) }, [state.messageFontSize])
  const [compactViewport, setCompactViewport] = useState(() => window.matchMedia('(max-width: 1000px)').matches)
  const trajectoryToggle = useRef<HTMLButtonElement>(null)
  const trajectoryPanel = useRef<HTMLElement>(null)
  const reducedMotion = useReducedMotion()
  const panelTransition = { duration: reducedMotion ? 0 : 0.16, ease: 'easeOut' as const }

  const closeTrajectory = () => {
    appStore.setTrajectoryOpen(false)
    requestAnimationFrame(() => trajectoryToggle.current?.focus())
  }
  useEffect(() => {
    const media = window.matchMedia('(max-width: 1000px)')
    const update = () => setCompactViewport(media.matches)
    media.addEventListener('change', update)
    return () => media.removeEventListener('change', update)
  }, [])
  useEffect(() => {
    if (!state.trajectoryOpen) return
    let frame = requestAnimationFrame(() => { frame = requestAnimationFrame(() => trajectoryPanel.current?.querySelector<HTMLButtonElement>('button')?.focus()) })
    return () => cancelAnimationFrame(frame)
  }, [state.trajectoryOpen, compactViewport])

  useEffect(() => {
    appStore.start()
    return () => appStore.stop()
  }, [])
  const sidebarWidth = state.sidebarCollapsed ? 0 : state.sidebarWidth
  const columns = `${sidebarWidth}px 0 minmax(0, 1fr)`
  const shellStyle = {
    gridTemplateColumns: columns,
    '--sidebar-width': `${state.sidebarWidth}px`,
  } as CSSProperties

  return (
    <div className="app-shell" style={shellStyle}>
      <Sidebar />
      {!state.sidebarCollapsed ? <ResizeSeparator value={state.sidebarWidth} onChange={(value) => appStore.setSidebarWidth(value)} /> : <div />}
      <div className="app-content" style={{ '--trajectory-width': `${state.trajectoryWidth}px` } as CSSProperties} onKeyDown={event => {
        if (!state.trajectoryOpen) return
        if (event.key === 'Escape') { event.stopPropagation(); closeTrajectory() }
        if (event.key !== 'Tab' || !compactViewport) return
        const nodes = [trajectoryToggle.current, ...(trajectoryPanel.current ? focusableElements(trajectoryPanel.current) : [])]
          .filter((node): node is HTMLElement => node !== null && node.getClientRects().length > 0)
        const first = nodes[0], last = nodes.at(-1)
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus() }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus() }
      }}>
        <SidebarToggle
          side="right"
          expanded={state.trajectoryOpen}
          controls="trajectory-panel"
          buttonRef={trajectoryToggle}
          className="trajectory-toggle"
          onClick={() => {
            if (state.trajectoryOpen) closeTrajectory()
            else appStore.setTrajectoryOpen(true)
          }}
        />
      <MainContent compactViewport={compactViewport} />
      <aside id="trajectory-panel" className={`trajectory-panel${state.trajectoryOpen ? ' is-open' : ''}`} aria-label="右侧栏" aria-hidden={!state.trajectoryOpen} inert={!state.trajectoryOpen} ref={trajectoryPanel}>
        <ResizeSeparator side="right" value={state.trajectoryWidth} onChange={(value) => appStore.setTrajectoryWidth(value)} />
        <AnimatePresence initial={false} mode="wait">
          {state.trajectoryOpen && (
            <motion.div
              key="trajectory"
              className="right-panel-content"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={panelTransition}
            >
              <Trajectory key={state.selectedSessionId} />
            </motion.div>
          )}
        </AnimatePresence>
      </aside>
      </div>
      <Settings state={state} />
    </div>
  )
}

const MainContent = memo(function MainContent({ compactViewport }: { compactViewport: boolean }) {
  const state = useAppStore(['selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'bootstrapLoad', 'bootstrap', 'sidebarCollapsed', 'trajectoryOpen', 'actionErrors', 'actionErrorOrigin', 'pendingActions'])
  const items = useMemo(() => buildTimeline(state.session, state.bootstrap?.userHome), [state.session, state.bootstrap?.userHome])
  const empty = state.selectedSessionId === null || (state.session !== null && items.length === 0)
  const workspaceSessions = appStore.sessions()
  // 页头与侧栏读同一份组级派生；任务尚未进入项目列表时它自成一例，只保留用户命名或“新任务”。
  const sessionTitle = state.session === null ? '选择一个任务' : sessionTitles(workspaceSessions)(
    workspaceSessions.find((session) => session.threadId === state.selectedSessionId) ?? state.session.summary,
  )
  const reducedMotion = useReducedMotion()
  const visibleError = state.actionErrorOrigin !== null && !hasInlineActionError(state.actionErrorOrigin)
    ? state.actionErrors[state.actionErrorOrigin]
    : null
  useEffect(() => {
    if (visibleError === null) return
    const timer = window.setTimeout(() => appStore.clearError(visibleError.origin), 5000)
    return () => window.clearTimeout(timer)
  }, [visibleError])

  return (
      <main className={`app-main${empty && state.bootstrapLoad.status === 'idle' ? ' is-empty' : ''}`} inert={compactViewport && state.trajectoryOpen} onPointerDownCapture={() => {
        if (!state.sidebarCollapsed && window.matchMedia('(max-width: 760px)').matches) appStore.toggleSidebar()
      }}>
        {!empty && <header className="conversation-header">
          <h1>{sessionTitle}</h1>
        </header>}
        {state.bootstrapLoad.status !== 'idle' ? (
          <section className="empty-state" aria-live="polite" aria-busy={state.bootstrapLoad.status === 'loading'}>
            <h2>{state.bootstrapLoad.status === 'loading' ? '正在读取工作台' : '工作台读取失败'}</h2>
            {state.bootstrapLoad.error && <>
              <p>{state.bootstrapLoad.error.message}</p>
              {state.bootstrapLoad.error.recovery && <p className="empty-recovery">{state.bootstrapLoad.error.recovery}</p>}
              <button type="button" className="secondary-button" onClick={() => void appStore.resync()}>重试读取工作台</button>
            </>}
          </section>
        ) : !empty && <Conversation state={state} items={items} />}
        <Composer centered={empty && state.bootstrapLoad.status === 'idle'} />
        <AnimatePresence>
          {visibleError !== null && <motion.div key="action-toast" className="action-toast" role="status"
            initial={{ opacity: reducedMotion ? 1 : 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
            transition={{ ...actionToastTransition, duration: reducedMotion ? 0 : actionToastTransition.duration }}>
            <strong>{visibleError.message}</strong>
            {visibleError.recovery && <span>{visibleError.recovery}</span>}
          </motion.div>}
        </AnimatePresence>
      </main>
  )
})

function ResizeSeparator({ value, onChange, side = 'left' }: { value: number; onChange: (value: number) => void; side?: 'left' | 'right' }) {
  const start = useRef<{ x: number; value: number } | null>(null)
  return (
    <div
      className={`resize-separator ${side === 'left' ? 'separator-sidebar' : 'separator-trajectory'}`}
      role="separator"
      aria-label={side === 'left' ? '调整侧栏宽度' : '调整轨迹侧栏宽度'}
      aria-orientation="vertical"
      aria-valuenow={value}
      tabIndex={0}
      onPointerDown={(event) => {
        event.currentTarget.setPointerCapture(event.pointerId)
        const width = side === 'right' ? event.currentTarget.parentElement?.getBoundingClientRect().width ?? value : value
        start.current = { x: event.clientX, value: width }
      }}
      onPointerMove={(event) => {
        if (!event.currentTarget.hasPointerCapture(event.pointerId) || start.current === null) return
        const delta = (event.clientX - start.current.x) * (side === 'right' ? -1 : 1)
        onChange(start.current.value + delta)
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId)
        start.current = null
      }}
      onKeyDown={(event) => {
        if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
        event.preventDefault()
        const direction = (event.key === 'ArrowRight' ? 1 : -1) * (side === 'right' ? -1 : 1)
        onChange(value + direction * 12)
      }}
    />
  )
}
