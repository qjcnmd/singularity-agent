import { actionToastTransition } from './motion'
import { focusableElements } from './interactions'
import { SidebarToggle } from './components/SidebarToggle'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { ContextPanel } from './components/ContextPanel'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Settings } from './components/Settings'
import { Sidebar } from './components/Sidebar'
import { BackgroundTasks, BackgroundTasksButton } from './components/BackgroundTasks'
import { hasInlineActionError, useAppStore, appStore } from './appStore'
import { buildTimeline } from './timeline'
import { sessionTitles } from './sessionTitle'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

export function App() {
  const state = useAppStore(['theme', 'messageFontSize', 'selectedSessionId', 'sidebarCollapsed', 'sidebarWidth', 'backgroundWidth', 'backgroundOpen'])
  useLayoutEffect(() => { document.documentElement.dataset.theme = state.theme }, [state.theme])
  useLayoutEffect(() => { document.documentElement.style.setProperty('--message-font-size', `${state.messageFontSize}px`) }, [state.messageFontSize])
  const [compactViewport, setCompactViewport] = useState(() => window.matchMedia('(max-width: 1000px)').matches)
  const [detailTab, setDetailTab] = useState<'context' | 'background'>('context')
  const detailOpener = useRef<HTMLButtonElement | null>(null)
  const backgroundToggle = useRef<HTMLButtonElement>(null)
  const backgroundPanel = useRef<HTMLElement>(null)
  const reducedMotion = useReducedMotion()
  const panelTransition = { duration: reducedMotion ? 0 : 0.16, ease: 'easeOut' as const }

  const closeBackground = () => {
    appStore.setBackgroundOpen(false)
    requestAnimationFrame(() => {
      const opener = detailOpener.current
      if (opener?.isConnected) opener.focus()
      else backgroundToggle.current?.focus()
    })
  }
  useEffect(() => {
    const media = window.matchMedia('(max-width: 1000px)')
    const update = () => setCompactViewport(media.matches)
    media.addEventListener('change', update)
    return () => media.removeEventListener('change', update)
  }, [])
  useEffect(() => {
    if (!state.backgroundOpen) return
    let frame = requestAnimationFrame(() => { frame = requestAnimationFrame(() => backgroundPanel.current?.querySelector<HTMLButtonElement>('button')?.focus()) })
    return () => cancelAnimationFrame(frame)
  }, [state.backgroundOpen, compactViewport])

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
      <div className="app-content" style={{ '--background-width': `${state.backgroundWidth}px` } as CSSProperties} onKeyDown={event => {
        if (!state.backgroundOpen) return
        if (event.key === 'Escape') { event.stopPropagation(); closeBackground() }
        if (event.key !== 'Tab' || !compactViewport) return
        const nodes = [backgroundToggle.current, ...(backgroundPanel.current ? focusableElements(backgroundPanel.current) : [])]
          .filter((node): node is HTMLElement => node !== null && node.getClientRects().length > 0)
        const first = nodes[0], last = nodes.at(-1)
        if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus() }
        else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus() }
      }}>
        <SidebarToggle
          side="right"
          expanded={state.backgroundOpen}
          controls="background-panel"
          buttonRef={backgroundToggle}
          className="background-toggle"
          onClick={() => {
            if (state.backgroundOpen) closeBackground()
            else {
              detailOpener.current = backgroundToggle.current
              appStore.setBackgroundOpen(true)
            }
          }}
        />
      <MainContent compactViewport={compactViewport} onOpenBackground={button => {
        setDetailTab('background')
        detailOpener.current = button
        appStore.setBackgroundOpen(true)
        requestAnimationFrame(() => backgroundPanel.current?.querySelector<HTMLButtonElement>('button')?.focus())
      }} />
      <aside id="background-panel" className={`background-panel${state.backgroundOpen ? ' is-open' : ''}`} aria-label="运行状态" aria-hidden={!state.backgroundOpen} inert={!state.backgroundOpen} ref={backgroundPanel}>
        <ResizeSeparator side="right" value={state.backgroundWidth} onChange={(value) => appStore.setBackgroundWidth(value)} />
        <AnimatePresence initial={false} mode="wait">
          {state.backgroundOpen && (
            <motion.div
              key="background"
              className="right-panel-content"
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={panelTransition}
            >
              <h2 className="detail-heading">运行状态</h2>
              <div className="detail-tabs" role="tablist" aria-label="运行状态分类">
                {(['context', 'background'] as const).map(tab => <button key={tab} type="button" role="tab" id={`detail-tab-${tab}`}
                  aria-selected={detailTab === tab} aria-controls="detail-body" tabIndex={detailTab === tab ? 0 : -1}
                  onClick={() => setDetailTab(tab)} onKeyDown={event => {
                    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
                    event.preventDefault()
                    const next = tab === 'context' ? 'background' : 'context'
                    setDetailTab(next)
                    document.getElementById(`detail-tab-${next}`)?.focus()
                  }}>{tab === 'context' ? '上下文' : '后台命令'}</button>)}
              </div>
              <div id="detail-body" className="detail-section" role="tabpanel" aria-labelledby={`detail-tab-${detailTab}`}>
                {detailTab === 'context' ? <ContextPanel /> : <BackgroundTasks />}
              </div>
            </motion.div>
          )}
        </AnimatePresence>
      </aside>
      </div>
      <Settings />
    </div>
  )
}

const MainContent = memo(function MainContent({ compactViewport, onOpenBackground }: {
  compactViewport: boolean
  onOpenBackground: (button: HTMLButtonElement) => void
}) {
  const state = useAppStore(['selectedSessionId', 'selectedWorkspaceId', 'session', 'sessionLoad', 'bootstrapLoad', 'bootstrap', 'sidebarCollapsed', 'backgroundOpen', 'actionErrors', 'actionErrorOrigin', 'pendingActions'])
  const items = useMemo(() => buildTimeline(state.session, state.bootstrap?.userHome), [state.session, state.bootstrap?.userHome])
  const empty = state.selectedSessionId === null || (state.session !== null && items.length === 0)
  const workspaceSessions = appStore.sessions()
  // 页头和侧栏用同一套标题规则；任务还不在项目列表里时，只显示用户起的名字或“新任务”。
  const sessionTitle = state.session === null ? '选择一个任务' : sessionTitles(workspaceSessions)(
    workspaceSessions.find((session) => session.threadId === state.selectedSessionId) ?? state.session.summary,
  )
  const reducedMotion = useReducedMotion()
  const visibleError = state.actionErrorOrigin !== null && !hasInlineActionError(state.actionErrorOrigin)
    ? state.actionErrors[state.actionErrorOrigin]
    : null
  useEffect(() => {
    const origin = state.actionErrorOrigin
    if (visibleError === null || origin === null) return
    const timer = window.setTimeout(() => appStore.clearError(origin), 5000)
    return () => window.clearTimeout(timer)
  }, [visibleError, state.actionErrorOrigin])

  return (
      <main className={`app-main${empty && state.bootstrapLoad.status === 'idle' ? ' is-empty' : ''}`} inert={compactViewport && state.backgroundOpen} onPointerDownCapture={() => {
        if (!state.sidebarCollapsed && window.matchMedia('(max-width: 760px)').matches) appStore.toggleSidebar()
      }}>
        {!empty && <header className="conversation-header">
          <h1>{sessionTitle}</h1>
          <BackgroundTasksButton onOpen={onOpenBackground} />
        </header>}
        {state.bootstrapLoad.status !== 'idle' ? (
          <section className="empty-state" aria-live="polite" aria-busy={state.bootstrapLoad.status === 'loading'}>
            <h2>{state.bootstrapLoad.status === 'loading' ? '正在读取项目列表' : '项目列表无法读取'}</h2>
            {state.bootstrapLoad.error && <>
              <p>{state.bootstrapLoad.error.message}</p>
              {state.bootstrapLoad.error.recovery && <p className="empty-recovery">{state.bootstrapLoad.error.recovery}</p>}
              <button type="button" className="secondary-button" onClick={() => void appStore.resync()}>重试</button>
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
      className={`resize-separator ${side === 'left' ? 'separator-sidebar' : 'separator-background'}`}
      role="separator"
      aria-label={side === 'left' ? '调整侧栏宽度' : '调整运行状态侧栏宽度'}
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
