import { focusableElements } from './interactions'
import { SidebarToggle } from './components/SidebarToggle'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { WorkspacePicker } from './components/WorkspacePicker'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Settings } from './components/Settings'
import { Sidebar } from './components/Sidebar'
import { Trajectory } from './components/Trajectory'
import { useWorkbenchStore, workbenchStore } from './store'
import { buildTimeline } from './timeline'
import { sessionDisplayTitle } from './sessionTitle'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

export function App() {
  const state = useWorkbenchStore(['theme', 'messageFontSize', 'selectedSessionId', 'bootstrap', 'sidebarCollapsed', 'sidebarWidth', 'trajectoryOpen', 'settingsOpen', 'actionErrors', 'pendingActions'])
  useLayoutEffect(() => { document.documentElement.dataset.theme = state.theme }, [state.theme])
  useLayoutEffect(() => { document.documentElement.style.setProperty('--message-font-size', `${state.messageFontSize}px`) }, [state.messageFontSize])
  const offeredModelSetup = useRef(false)
  const [initialSetup, setInitialSetup] = useState(false)
  const [compactViewport, setCompactViewport] = useState(() => window.matchMedia('(max-width: 1000px)').matches)
  const trajectoryToggle = useRef<HTMLButtonElement>(null)
  const trajectoryPanel = useRef<HTMLElement>(null)
  const [rightPanelView, setRightPanelView] = useState<'choices' | 'trajectory'>('choices')
  const reducedMotion = useReducedMotion()
  const panelTransition = { duration: reducedMotion ? 0 : 0.16, ease: 'easeOut' as const }

  const closeTrajectory = () => {
    workbenchStore.setTrajectoryOpen(false)
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
    workbenchStore.start()
    return () => workbenchStore.stop()
  }, [])
  useEffect(() => {
    if (state.bootstrap === null || offeredModelSetup.current) return
    offeredModelSetup.current = true
    if (state.bootstrap.modelCatalog.configuration !== 'ready') { setInitialSetup(true); workbenchStore.setSettingsOpen(true) }
  }, [state.bootstrap])
  const sidebarWidth = state.sidebarCollapsed ? 0 : state.sidebarWidth
  const columns = `${sidebarWidth}px 0 minmax(0, 1fr)`
  const shellStyle = {
    gridTemplateColumns: columns,
    '--sidebar-effective': `${sidebarWidth}px`,
    '--sidebar-width': `${state.sidebarWidth}px`,
  } as CSSProperties

  return (
    <div className="app-shell" style={shellStyle}>
      <Sidebar />
      {!state.sidebarCollapsed ? <ResizeSeparator value={state.sidebarWidth} onChange={(value) => workbenchStore.setSidebarWidth(value)} /> : <div />}
      <div className="workbench-content" style={{ '--trajectory-width': `${state.sidebarWidth}px` } as CSSProperties} onKeyDown={event => {
        if (!state.trajectoryOpen) return
        if (event.key === 'Escape') { event.stopPropagation(); closeTrajectory() }
        if (event.key !== 'Tab' || !compactViewport) return
        const nodes = [trajectoryToggle.current, ...(trajectoryPanel.current ? focusableElements(trajectoryPanel.current) : [])]
          .filter((node): node is HTMLElement => node !== null && node.getClientRects().length > 0)
        const first = nodes[0], last = nodes.at(-1)
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus() }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus() }
      }}>
        <SidebarToggle side="right" expanded={state.trajectoryOpen} controls="trajectory-panel" buttonRef={trajectoryToggle} className="trajectory-toggle" onClick={() => { if (state.trajectoryOpen) closeTrajectory(); else { setRightPanelView('choices'); workbenchStore.setTrajectoryOpen(true) } }} />
      <MainContent compactViewport={compactViewport} />
      <aside id="trajectory-panel" className={`trajectory-panel${state.trajectoryOpen ? ' is-open' : ''}`} aria-label="右侧栏" aria-hidden={!state.trajectoryOpen} inert={!state.trajectoryOpen} ref={trajectoryPanel}>
        <ResizeSeparator side="right" value={state.sidebarWidth} onChange={(value) => workbenchStore.setSidebarWidth(value)} />
        <header className="trajectory-panel-header">
          {rightPanelView === 'trajectory' ? <button type="button" className="quiet-button" aria-label="返回侧栏选择" onClick={() => setRightPanelView('choices')}>← 轨迹</button> : <span />}
        </header>
        <AnimatePresence initial={false} mode="wait">
          {rightPanelView === 'choices' ? <motion.div key="choices" className="right-panel-choices" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} transition={panelTransition} onAnimationComplete={definition => { if (state.trajectoryOpen && typeof definition === 'object' && 'opacity' in definition && definition.opacity === 1) trajectoryPanel.current?.querySelector<HTMLButtonElement>('.right-panel-choice')?.focus() }}>
            <button type="button" className="right-panel-choice" onClick={() => setRightPanelView('trajectory')}>
              <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" aria-hidden="true"><path d="M8 6h12M8 12h12M8 18h12" /><circle cx="3" cy="6" r="1" /><circle cx="3" cy="12" r="1" /><circle cx="3" cy="18" r="1" /></svg>
              <span>轨迹</span>
            </button>
          </motion.div> : <motion.div key="trajectory" className="right-panel-content" initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} transition={panelTransition} onAnimationComplete={definition => { if (state.trajectoryOpen && typeof definition === 'object' && 'opacity' in definition && definition.opacity === 1) trajectoryPanel.current?.querySelector<HTMLButtonElement>('[aria-label="返回侧栏选择"]')?.focus() }}>
            <Trajectory key={state.selectedSessionId} visible={state.trajectoryOpen} />
          </motion.div>}
        </AnimatePresence>
      </aside>
      </div>
      <Settings state={state} initialSetup={initialSetup} onSetupDone={() => setInitialSetup(false)} />
    </div>
  )
}

const MainContent = memo(function MainContent({ compactViewport }: { compactViewport: boolean }) {
  const state = useWorkbenchStore(['selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'bootstrap', 'sidebarCollapsed', 'trajectoryOpen', 'actionError', 'pendingActions', 'viewportAnchors', 'workspaceAppearance'])
  const items = useMemo(() => buildTimeline(state.session), [state.session])
  const empty = state.selectedSessionId === null || (state.session !== null && items.length === 0)
  const workspaceSessions = workbenchStore.sessions()
  const sessionTitle = state.session === null ? '选择一个任务' : sessionDisplayTitle(
    workspaceSessions.find((session) => session.threadId === state.selectedSessionId) ?? state.session.history.summary,
    workspaceSessions,
  )
  const visibleError = state.actionError !== null && state.actionError.code !== 'unavailable' && (state.actionError.origin === 'directory:picker' || !/^(control|provider|provider-key|directory|file-search):/.test(state.actionError.origin))
    ? state.actionError
    : null
  useEffect(() => {
    if (visibleError === null) return
    const timer = window.setTimeout(() => workbenchStore.clearError(visibleError.origin), 5000)
    return () => window.clearTimeout(timer)
  }, [visibleError])

  return (
      <main className={`workbench-main${empty ? ' is-empty' : ''}`} inert={compactViewport && state.trajectoryOpen} onPointerDownCapture={() => {
        if (!state.sidebarCollapsed && window.matchMedia('(max-width: 760px)').matches) workbenchStore.toggleSidebar()
      }}>
        <header className="conversation-header">
          <div className="conversation-title">
            <div className="title-line">
              {!empty && <h1>{sessionTitle}</h1>}
            </div>
          </div>
        </header>
        {empty ? <div className="new-session-hero">
          <h1>准备做什么？</h1>
          <WorkspacePicker state={state} />
        </div> : <Conversation state={state} items={items} />}
        <Composer />
        {visibleError !== null && <div className="action-toast" role="status">{visibleError.message}</div>}
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
