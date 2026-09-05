import { useEffect, useMemo, useRef, useState, type CSSProperties } from 'react'
import { formatElapsed, phaseText } from './copy'
import { useSelectionGuard } from './interactions'
import { Composer } from './components/Composer'
import { Conversation } from './components/Conversation'
import { Details } from './components/Details'
import { DirectoryPicker } from './components/DirectoryPicker'
import { Help } from './components/Help'
import { Settings } from './components/Settings'
import { Sidebar } from './components/Sidebar'
import { useWorkbenchStore, workbenchStore } from './store'
import { buildTimeline } from './timeline'
import { sessionDisplayTitle } from './sessionTitle'

export function App() {
  const state = useWorkbenchStore()
  const offeredModelSetup = useRef(false)
  const [now, setNow] = useState(Date.now())
  const headerGuard = useSelectionGuard()

  useEffect(() => {
    workbenchStore.start()
    return () => workbenchStore.stop()
  }, [])
  useEffect(() => {
    if (state.bootstrap === null || offeredModelSetup.current) return
    offeredModelSetup.current = true
    if (state.bootstrap.modelCatalog.configuration !== 'ready') workbenchStore.setSettingsOpen(true)
  }, [state.bootstrap])
  useEffect(() => {
    const active = state.session?.runtime.phase
    if (active !== 'running' && active !== 'stopping' && active !== 'compacting') return
    setNow(Date.now())
    const timer = window.setInterval(() => setNow(Date.now()), 1_000)
    return () => window.clearInterval(timer)
  }, [state.session?.runtime.phase])

  const items = useMemo(() => buildTimeline(state.session), [state.session])
  const detailItem = items.find((item) => item.key === state.detailsItemId) ?? null
  const sidebarWidth = state.sidebarCollapsed ? 52 : state.sidebarWidth
  const columns = state.detailsOpen
    ? `${sidebarWidth}px 5px minmax(0, 1fr) 5px ${state.detailsWidth}px`
    : `${sidebarWidth}px 5px minmax(0, 1fr)`
  const shellStyle = {
    gridTemplateColumns: columns,
    '--details-width': `${state.detailsWidth}px`,
    '--sidebar-effective': `${sidebarWidth}px`,
  } as CSSProperties
  const selectedWorkspace = state.bootstrap?.workspaces.find((item) => item.workspaceId === state.selectedWorkspaceId)
  const workspaceSessions = state.selectedWorkspaceId === null ? [] : state.bootstrap?.sessionsByWorkspace[state.selectedWorkspaceId] ?? []
  const sessionTitle = state.session === null ? '选择一个任务' : sessionDisplayTitle(
    workspaceSessions.find((session) => session.threadId === state.selectedSessionId) ?? state.session.summary,
    workspaceSessions,
  )
  const phase = state.session?.runtime.phase
  const startedAt = state.session?.runtime.activeTurn?.startedAt ?? state.session?.runtime.activeCompaction?.startedAt
  const elapsed = phase !== undefined && phase !== 'idle' && phase !== 'reserved' ? formatElapsed(startedAt, now) : ''
  const visibleError = state.actionError !== null && !/^(control|provider|provider-key|directory|file-search):/.test(state.actionError.origin)
    ? state.actionError
    : null

  return (
    <div className="app-shell" style={shellStyle}>
      <Sidebar state={state} />
      <ResizeSeparator side="sidebar" value={state.sidebarWidth} onChange={(value) => workbenchStore.setSidebarWidth(value)} />
      <main className="workbench-main">
        <header className="conversation-header">
          <div className="conversation-title">
            <div className="title-line">
              <h1>{sessionTitle}</h1>
              {phase !== undefined && (
                <span className={`phase-badge phase-${phase}`} role="status" aria-live="polite">
                  <span className="phase-dot" aria-hidden="true" />{phaseText[phase]}{elapsed !== '' && ` · ${elapsed}`}
                </span>
              )}
            </div>
            <p>
              <span>{selectedWorkspace?.name ?? '本地工作台'}</span>
              {state.session !== null && <><span aria-hidden="true">/</span><span>{state.session.summary.turnCount} 回合</span></>}
            </p>
          </div>
          <div className="header-actions">
            <button type="button" className="header-button" aria-label="帮助" title="帮助" {...headerGuard(() => workbenchStore.setHelpOpen(true))}><span aria-hidden="true">?</span><span>帮助</span></button>
            <button
              type="button"
              className="header-button"
              aria-label="详情"
              title="详情"
              aria-pressed={state.detailsOpen}
              {...headerGuard(() => state.detailsOpen
                ? workbenchStore.closeDetails()
                : workbenchStore.selectDetails(items.findLast((item) => item.kind !== 'user' && item.kind !== 'assistant' && item.kind !== 'terminal')?.key ?? null))}
            >
              <span aria-hidden="true">▤</span><span>详情</span>
            </button>
          </div>
        </header>
        <div className="notice-slot">
          {!state.guideCollapsed && state.bootstrap !== null && (
            <section className="first-use-guide" aria-label="快速说明">
              <span className="guide-mark" aria-hidden="true">i</span>
              <p>{state.selectedWorkspaceId === null
                ? <><strong>先从左侧添加项目。</strong>选择本地文件夹后新建任务；Agent 使用当前账户，拥有完整本机权限。</>
                : state.selectedSessionId === null
                  ? <><strong>新建任务后即可开始。</strong>输入 <kbd>@</kbd> 引用项目文件；运行中可随时停止、即时转向或排入后续消息。</>
                  : <><strong>项目提供上下文，Agent 拥有完整本机权限。</strong>输入 <kbd>@</kbd> 引用文件；运行中可停止、即时转向或将消息排到下一回合。</>}</p>
              <button type="button" className="quiet-button" onClick={() => workbenchStore.setGuideCollapsed(true)}>知道了</button>
            </section>
          )}
          {visibleError !== null && (
            <div className="error-banner" role="alert">
              <div><strong>{visibleError.message}</strong><span>{visibleError.recovery}</span></div>
              <button type="button" className="icon-button" onClick={() => workbenchStore.clearError(visibleError.origin)} aria-label="关闭错误">×</button>
            </div>
          )}
        </div>
        <Conversation state={state} items={items} />
        <Composer state={state} />
      </main>
      {state.detailsOpen && (
        <>
          <ResizeSeparator side="details" value={state.detailsWidth} onChange={(value) => workbenchStore.setDetailsWidth(value)} />
          <Details item={detailItem} onClose={() => workbenchStore.closeDetails()} />
        </>
      )}
      <DirectoryPicker picker={state.directoryPicker} />
      <Settings state={state} />
      <Help open={state.helpOpen} />
    </div>
  )
}

function ResizeSeparator({ side, value, onChange }: { side: 'sidebar' | 'details'; value: number; onChange: (value: number) => void }) {
  const start = useRef<{ x: number; value: number } | null>(null)
  return (
    <div
      className={`resize-separator separator-${side}`}
      role="separator"
      aria-label={side === 'sidebar' ? '调整侧栏宽度' : '调整详情栏宽度'}
      aria-orientation="vertical"
      aria-valuenow={value}
      tabIndex={0}
      onPointerDown={(event) => {
        event.currentTarget.setPointerCapture(event.pointerId)
        start.current = { x: event.clientX, value }
      }}
      onPointerMove={(event) => {
        if (!event.currentTarget.hasPointerCapture(event.pointerId) || start.current === null) return
        const delta = event.clientX - start.current.x
        onChange(start.current.value + (side === 'sidebar' ? delta : -delta))
      }}
      onPointerUp={(event) => {
        if (event.currentTarget.hasPointerCapture(event.pointerId)) event.currentTarget.releasePointerCapture(event.pointerId)
        start.current = null
      }}
      onKeyDown={(event) => {
        if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
        event.preventDefault()
        const direction = event.key === 'ArrowRight' ? 1 : -1
        onChange(value + direction * (side === 'sidebar' ? 12 : -12))
      }}
    />
  )
}
