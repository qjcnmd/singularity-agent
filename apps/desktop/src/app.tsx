import { workspaceKey } from './workspace'
import { workspaceName } from './copy'
import { actionToastTransition } from './motion'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Settings } from './components/Settings'
import { Sidebar } from './components/Sidebar'
import { RunPanel } from './components/RunPanel'
import { InstructionsPage, SkillsPage, McpPage } from './components/ManagePages'
import { hasInlineActionError, useAppStore, appStore } from './appStore'
import { buildHistoryRows, buildTimeline } from './timeline'
import { bubbleVariables } from './bubbleColors'
import { clampFilePanelWidth } from './viewPersistence'
import { FilePanel } from './components/FilePanel'
import { sessionTitles } from './sessionTitle'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

export function App() {
  const state = useAppStore(['theme', 'messageFontSize', 'assistantBubble', 'userBubble', 'sidebarCollapsed', 'sidebarWidth', 'fileTabs', 'filePanelWidth'])
  const [accent, setAccent] = useState(() => getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())
  const filePanelOpen = state.fileTabs.length > 0
  useLayoutEffect(() => { document.documentElement.dataset.theme = state.theme }, [state.theme])
  useLayoutEffect(() => { document.documentElement.style.setProperty('--message-font-size', `${state.messageFontSize}px`) }, [state.messageFontSize])
  useLayoutEffect(() => {
    for (const [name, value] of Object.entries(bubbleVariables(state))) document.documentElement.style.setProperty(name, value)
    // CSS 是主题色的唯一来源；写入主题与气泡变量后再把解析值交给画布。
    setAccent(getComputedStyle(document.documentElement).getPropertyValue('--accent').trim())
  }, [state.theme, state.assistantBubble, state.userBubble])
  useEffect(() => { appStore.start(); return () => appStore.stop() }, [])
  const style = {
    gridTemplateColumns: `${state.sidebarCollapsed ? 0 : state.sidebarWidth}px 0 minmax(0, 1fr) 0 ${filePanelOpen ? clampFilePanelWidth(state.filePanelWidth) : 0}px`,
    '--sidebar-width': `${state.sidebarWidth}px`,
  } as CSSProperties
  return <div className="app-shell" style={style}>
    <Sidebar />
    {!state.sidebarCollapsed ? <ResizeSeparator label="调整侧栏宽度" value={state.sidebarWidth} onChange={value => appStore.setSidebarWidth(value)} /> : <div />}
    <MainContent accent={accent} />
    {filePanelOpen ? <ResizeSeparator label="调整文件栏宽度" reversed value={clampFilePanelWidth(state.filePanelWidth)} onChange={value => appStore.setFilePanelWidth(value)} /> : <div />}
    <FilePanel />
    <Settings />
  </div>
}

const MainContent = memo(function MainContent({ accent }: { accent: string }) {
  const state = useAppStore(['page', 'liveSessions', 'selectedSessionId', 'selectedWorkspaceRoot', 'session', 'sessionLoad', 'bootstrapLoad', 'bootstrap', 'sidebarCollapsed', 'backgroundOpen', 'actionErrors', 'actionErrorOrigin'])
  const facts = state.session?.facts
  const runtime = state.session?.runtime
  const failure = runtime?.failure
  const pendingQuestion = runtime?.pendingQuestion
  const pendingMessages = runtime?.pendingMessages
  const failureFromRuntime = failure?.source === 'turn'
  const historyRows = useMemo(() => buildHistoryRows(facts, failureFromRuntime), [facts, failureFromRuntime])
  // 这里只依赖影响临时条目的字段；上下文用量、阶段和后台状态不改变排列。
  const items = useMemo(() => buildTimeline(historyRows, facts, runtime), [historyRows, facts, failure, pendingQuestion, pendingMessages])
  const empty = state.selectedSessionId === null || (state.session !== null && items.length === 0)
  const workspace = state.bootstrap?.workspaces.find(workspace => workspaceKey(workspace.root) === state.selectedWorkspaceRoot)
  const workspaceSessions = appStore.sessions()
  // 页头和侧栏用同一套标题规则；任务还不在项目列表里时，只显示用户起的名字或“新任务”。
  const sessionTitle = state.session === null ? '' : sessionTitles(workspaceSessions)(
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
      <main className={`app-main${state.page !== 'chat' ? ' is-manage' : empty && state.bootstrapLoad.status === 'idle' ? ' is-empty' : ''}${state.page === 'chat' && state.selectedSessionId && state.backgroundOpen ? ' has-panel' : ''}`} onPointerDownCapture={() => {
        if (!state.sidebarCollapsed && window.matchMedia('(max-width: 760px)').matches) appStore.toggleSidebar()
      }}>
        <header className="conversation-header">
          {state.page === 'chat' && state.selectedSessionId && <h1>{sessionTitle}{workspace && <small>{workspaceName(workspace)}</small>}</h1>}
          {state.page === 'chat' && state.selectedSessionId && <RunPanel key={state.selectedSessionId} />}
        </header>
        {state.page === 'instructions' ? <InstructionsPage /> : state.page === 'skills' ? <SkillsPage /> : state.page === 'mcp' ? <McpPage /> : <>
        {state.bootstrapLoad.status !== 'idle' ? (
          <section className="empty-state" aria-live="polite" aria-busy={state.bootstrapLoad.status === 'loading'}>
            <h2>{state.bootstrapLoad.status === 'loading' ? '正在读取项目列表' : '项目列表无法读取'}</h2>
            {state.bootstrapLoad.error && <>
              <p>{state.bootstrapLoad.error.message}</p>
              <button type="button" className="secondary-button" onClick={() => void appStore.resync()}>重试</button>
            </>}
          </section>
        ) : !empty && <Conversation state={state} items={items} />}
        <Composer accent={accent} />
        </>}
        <AnimatePresence>
          {visibleError !== null && <motion.div key="action-toast" className="action-toast" role="status"
            initial={{ opacity: reducedMotion ? 1 : 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }}
            transition={reducedMotion ? { duration: 0 } : actionToastTransition}>
            <strong>{visibleError.message}</strong>
          </motion.div>}
        </AnimatePresence>
      </main>
  )
})
/** 拖动或用左右方向键调宽度；reversed 用于右侧的栏，往左拖变宽。 */
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
