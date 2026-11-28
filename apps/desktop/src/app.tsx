import { workspaceKey } from './workspace'
import { APP_NAME } from './identity'
import { workspaceName } from './copy'
import { pop } from './motion'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Settings } from './components/Settings'
import { Sidebar, SidebarToggle } from './components/Sidebar'
import { RunPanel } from './components/RunPanel'
import { hasInlineActionError, useAppStore, appStore } from './appStore'
import { buildHistoryRows, buildTimeline, formatSessionMetrics } from './timeline'
import { bubbleVariables } from './bubbleColors'
import { clampFilePanelWidth } from './viewPersistence'
import { FilePanel } from './components/FilePanel'
import { sessionTitles } from './sessionTitle'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

export function App() {
  const state = useAppStore(['theme', 'messageFontSize', 'accentColor', 'sidebarCollapsed', 'sidebarWidth', 'fileTabs', 'filePanelWidth'])
  const [, setViewportWidth] = useState(() => window.innerWidth)
  const filePanelOpen = state.fileTabs.length > 0
  const variables = bubbleVariables(state.accentColor)
  useLayoutEffect(() => {
    document.documentElement.dataset.theme = state.theme
    void window.desktop.setTheme(state.theme)
  }, [state.theme])
  useLayoutEffect(() => { document.documentElement.style.setProperty('--message-font-size', `${state.messageFontSize}px`) }, [state.messageFontSize])
  useLayoutEffect(() => {
    for (const [name, value] of Object.entries(variables)) document.documentElement.style.setProperty(name, value)
  }, [variables])
  useEffect(() => {
    if (!filePanelOpen) return
    const onResize = () => setViewportWidth(window.innerWidth)
    window.addEventListener('resize', onResize)
    return () => window.removeEventListener('resize', onResize)
  }, [filePanelOpen])
  const clampedFilePanelWidth = clampFilePanelWidth(state.filePanelWidth)
  const style = {
    gridTemplateColumns: `${state.sidebarCollapsed ? 0 : state.sidebarWidth}px 0 minmax(0, 1fr) ${filePanelOpen ? 8 : 0}px ${filePanelOpen ? clampedFilePanelWidth : 0}px`,
    '--sidebar-width': `${state.sidebarWidth}px`,
    '--file-panel-width': `${clampedFilePanelWidth}px`,
  } as CSSProperties
  return <div className="app-frame">
    <div className="window-titlebar">
      <span className="window-titlebar-name">{APP_NAME}</span>
    </div>
    <div className="app-shell" style={style}>
      <Sidebar />
      {!state.sidebarCollapsed ? <ResizeSeparator label="调整侧栏宽度" value={state.sidebarWidth} onChange={value => appStore.setSidebarWidth(value)} /> : <div />}
      <MainContent filePanelOpen={filePanelOpen} />
      {filePanelOpen ? <ResizeSeparator label="调整文件栏宽度" reversed value={clampedFilePanelWidth} onChange={value => appStore.setFilePanelWidth(value)} /> : <div />}
      <FilePanel />
      <SidebarToggle
        className="sidebar-fixed-toggle"
        expanded={!state.sidebarCollapsed}
        controls="left-sidebar"
        onClick={() => appStore.toggleSidebar()}
      />
      <Settings />
    </div>
  </div>
}

const MainContent = memo(function MainContent({ filePanelOpen }: { filePanelOpen: boolean }) {
  const state = useAppStore(['selectedSessionId', 'selectedWorkspaceRoot', 'session', 'sessionLoad', 'bootstrapLoad', 'bootstrap', 'sidebarCollapsed', 'backgroundOpen', 'actionErrors', 'actionErrorOrigin'])
  const facts = state.session?.facts
  const runtime = state.session?.runtime
  const failure = runtime?.failure
  const pendingQuestion = runtime?.pendingQuestion
  const pendingMessages = runtime?.pendingMessages
  const failureFromRuntime = failure?.source === 'turn'
  const historyRows = useMemo(() => buildHistoryRows(facts, failureFromRuntime), [facts, failureFromRuntime])
  const items = useMemo(
    () => buildTimeline(historyRows, facts, runtime),
    [historyRows, facts, failure, pendingQuestion, pendingMessages],
  )
  const empty = state.selectedSessionId === null || (state.session !== null && items.length === 0)
  const workspace = state.bootstrap?.workspaces.find(workspace => workspaceKey(workspace.root) === state.selectedWorkspaceRoot)
  const workspaceSessions = appStore.sessions()
  const sessionTitle = state.session === null ? '' : sessionTitles(workspaceSessions)(
    workspaceSessions.find((session) => session.threadId === state.selectedSessionId) ?? state.session.summary,
  )
  const sessionMetrics = state.selectedSessionId ? formatSessionMetrics(state.session?.runtime.modelUsage) : ''
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
    <div
      className="main-column"
      onPointerDownCapture={() => {
        if (!state.sidebarCollapsed && window.matchMedia('(max-width: 760px)').matches) appStore.toggleSidebar()
      }}
    >
      <header className="conversation-header">
        {state.selectedSessionId && <h1>{sessionTitle}{workspace && <small>{workspaceName(workspace)}</small>}</h1>}
        {sessionMetrics && (
          <span className="session-metrics" title="平均首 token 时间 · 平均输出速度 · 会话总用量 · 缓存命中率">
            {sessionMetrics}
          </span>
        )}
      </header>
      {state.selectedSessionId && <RunPanel key={state.selectedSessionId} overlay={filePanelOpen} />}
      <main className={`app-main${empty && state.bootstrapLoad.status === 'idle' ? ' is-empty' : ''}${state.selectedSessionId && state.backgroundOpen && !filePanelOpen ? ' has-panel' : ''}`}>
        {state.bootstrapLoad.status !== 'idle' ? (
          <section className="empty-state" aria-live="polite" aria-busy={state.bootstrapLoad.status === 'loading'}>
            <h2>{state.bootstrapLoad.status === 'loading' ? '正在读取项目列表' : '项目列表无法读取'}</h2>
            {state.bootstrapLoad.error && <>
              <p>{state.bootstrapLoad.error.message}</p>
              <button type="button" className="secondary-button" onClick={() => void appStore.resync()}>重试</button>
            </>}
          </section>
        ) : !empty && <Conversation state={state} items={items} />}
        <Composer />
        <AnimatePresence>
          {visibleError !== null && <motion.div key="action-toast" className="action-toast" role="status"
            initial={{ opacity: reducedMotion ? 1 : 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
            transition={reducedMotion ? { duration: 0 } : pop}>
            <strong>{visibleError.message}</strong>
          </motion.div>}
        </AnimatePresence>
      </main>
    </div>
  )
})
function ResizeSeparator({ label, value, onChange, reversed = false }: { label: string; value: number; onChange: (value: number) => void; reversed?: boolean }) {
  const start = useRef<{ x: number; value: number } | null>(null)
  return (
    <div
      className="resize-separator"
      role="separator"
      aria-label={label}
      aria-orientation="vertical"
      aria-valuenow={value}
      tabIndex={0}
      onPointerDown={(event) => {
        event.currentTarget.setPointerCapture(event.pointerId)
        start.current = { x: event.clientX, value }
      }}
      onPointerMove={(event) => {
        if (!event.currentTarget.hasPointerCapture(event.pointerId) || start.current === null) return
        const delta = (event.clientX - start.current.x) * (reversed ? -1 : 1)
        onChange(start.current.value + delta)
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId)
        start.current = null
      }}
      onKeyDown={(event) => {
        if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
        event.preventDefault()
        const direction = (event.key === 'ArrowRight' ? 1 : -1) * (reversed ? -1 : 1)
        onChange(value + direction * 12)
      }}
    />
  )
}
