import { SelectionSlider } from './SelectionSlider'
import { workspaceKey } from '../workspace'
import { X, Minus, Plus, Folder, FolderOpen } from 'lucide-react'
import { SidebarToggle } from './SidebarToggle'
import { memo, useEffect, useLayoutEffect, useId, useRef, useState } from 'react'
import { actionOrigin, appStore, pendingKey, useAppStore, type AppState } from '../appStore'
import type { ThreadSummary, Workspace } from '../protocol'
import { sessionTitles } from '../sessionTitle'
import { workspaceName } from '../copy'
import { sessionState } from '../sessionState'
import { Dialog } from './Dialog'
import { Menu } from './Menu'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

type PendingDialog =
  | { kind: 'none' }
  | { kind: 'rename'; session: ThreadSummary }
  | { kind: 'remove'; workspace: Workspace }

export const Sidebar = memo(SidebarView)

const sidebarFields = ['bootstrap', 'liveSessions', 'selectedSessionId', 'selectedWorkspaceRoot', 'sidebarCollapsed', 'sidebarView', 'unreadSessions', 'page'] as const
type SidebarState = Pick<AppState, typeof sidebarFields[number]>

/** 浅比较侧栏依赖的状态，避免无关后台事件触发重渲染。 */
function sameSidebarState(previous: SidebarState, next: SidebarState): boolean {
  return sidebarFields.every(key => {
    if (key !== 'liveSessions') return Object.is(previous[key], next[key])
    const left = previous.liveSessions, right = next.liveSessions
    return Object.keys(left).length === Object.keys(right).length && Object.entries(left).every(([id, value]) =>
      value.phase === right[id]?.phase && value.failure?.source === right[id]?.failure?.source
)
  })
}

function SidebarView() {
  const state = useAppStore(sidebarFields, sameSidebarState)
  const sidebar = useRef<HTMLElement>(null)
  const sidebarFocus = useRef<string | null>(null)
  const focusSidebar = () => {
    const target = sidebarFocus.current && sidebar.current?.querySelector<HTMLButtonElement>(sidebarFocus.current)
    if (!target) return
    target.focus()
    if (document.activeElement === target) sidebarFocus.current = null
  }
  const toggleSidebar = () => {
    sidebarFocus.current = state.sidebarCollapsed ? '.sidebar-expanded .sidebar-brand button' : '.sidebar-rail button'
    appStore.toggleSidebar()
  }
  useLayoutEffect(() => {
    const target = sidebarFocus.current
    if (!target) return
    const frame = requestAnimationFrame(focusSidebar)
    return () => cancelAnimationFrame(frame)
  }, [state.sidebarCollapsed])
  const [dialog, setDialog] = useState<PendingDialog>({ kind: 'none' })
  const collapsed = new Set(state.sidebarView.collapsed)
  const [showAll, setShowAll] = useState<Set<string>>(new Set())
  return (
    <>
            <aside ref={sidebar} className={`sidebar-shell${state.sidebarCollapsed ? ' is-collapsed' : ''}`} aria-label="项目与任务导航" onTransitionEnd={focusSidebar}>
      <div className="sidebar sidebar-rail" inert={!state.sidebarCollapsed} aria-hidden={!state.sidebarCollapsed}>
        <SidebarToggle expanded={!state.sidebarCollapsed} controls="left-sidebar" onClick={toggleSidebar} />

      </div>
      <div id="left-sidebar" className="sidebar sidebar-expanded" inert={state.sidebarCollapsed} aria-hidden={state.sidebarCollapsed}>
        <header className="sidebar-brand">
          <SidebarToggle expanded={!state.sidebarCollapsed} controls="left-sidebar" onClick={toggleSidebar} />
        </header>

        <nav className="side-nav">
          <button className={`nav-item${state.page === 'chat' && state.selectedSessionId === null ? ' is-active' : ''}`} type="button" onClick={() => appStore.newTask()}>新建任务</button>
          {([{ page: 'instructions', label: '自定义指令' }, { page: 'skills', label: 'Skills' }, { page: 'mcp', label: 'MCP' }] as const).map(({ page, label }) =>
            <button key={page} type="button" className={`nav-item${state.page === page ? ' is-active' : ''}`} onClick={() => appStore.setPage(page)}>{label}</button>)}
        </nav>
        <section className="sidebar-section workspace-section">
          <div className="section-heading">
            <span>项目</span>
            <button type="button" className="icon-button section-action" onClick={() => appStore.openDirectoryPicker()} aria-label="添加项目"><Plus size={16} strokeWidth={1.6} /></button>
          </div>
          <div className="workspace-list">
            {state.bootstrap?.workspaces.map((item) => {
              const root = workspaceKey(item.root)
              const sessions = state.bootstrap?.sessionsByWorkspace[root] ?? []
              const titleOf = sessionTitles(sessions)
              const expanded = !collapsed.has(root)
              const visible = showAll.has(root) ? sessions : sessions.slice(0, 5)
              return <section key={root}>
                <WorkspaceButton
                  workspace={item}
                  selected={root === state.selectedWorkspaceRoot && state.selectedSessionId === null && state.page === 'chat'}
                  expanded={expanded}
                  onToggle={() => {
                    const next = new Set(collapsed)
                    if (next.has(root)) {
                      next.delete(root)
                      setShowAll(previous => {
                        const reset = new Set(previous)
                        reset.delete(root)
                        return reset
                      })
                    } else next.add(root)
                    appStore.setSidebarView({ collapsed: [...next] })
                  }}
                  onRemove={() => setDialog({ kind: 'remove', workspace: item })}
                />
                <Disclosure open={expanded}><SessionList activeKey={state.page === 'chat' ? state.selectedSessionId : null}>
                  {visible.map(session => <SessionButton key={session.threadId} session={session} title={titleOf(session)}
                    selected={state.page === 'chat' && session.threadId === state.selectedSessionId} live={state.liveSessions[session.threadId]}
                    unread={state.unreadSessions.has(session.threadId)} onRename={() => setDialog({ kind: 'rename', session })}
                    onDelete={() => { void appStore.deleteSession(session.threadId) }} />)}
                  {sessions.length > 5 && (
                    <button
                      type="button"
                      className="quiet-button"
                      onClick={() => setShowAll(previous => {
                        const next = new Set(previous)
                        if (next.has(root)) next.delete(root)
                        else next.add(root)
                        return next
                      })}
                    >
                      {showAll.has(root) ? '收起' : `显示更多 (${sessions.length - 5})`}
                    </button>
                  )}
                </SessionList></Disclosure>
              </section>
            })}
          </div>
        </section>
        <footer className="sidebar-footer">
          <button type="button" className="nav-item" onClick={() => appStore.setSettingsOpen(true)}>设置</button>
        </footer>
      </div></aside>
      <SidebarDialog state={dialog} onClose={() => setDialog(current => current === dialog ? { kind: 'none' } : current)} />
    </>
  )
}

function WorkspaceButton({ workspace, selected, expanded, onToggle, onRemove }: { workspace: Workspace; selected: boolean; expanded: boolean; onToggle: () => void; onRemove: () => void }) {
  const name = workspaceName(workspace)
  return (
    <div className={`workspace-row${selected ? ' is-selected' : ''}`}>
      <button
        type="button"
        className="workspace-button"
        onClick={onToggle}
        aria-expanded={expanded}
      >
        {expanded ? <FolderOpen size={16} strokeWidth={1.5} className="workspace-folder" aria-hidden="true" /> : <Folder size={16} strokeWidth={1.5} className="workspace-folder" aria-hidden="true" />}
        <span className="workspace-name"><strong>{name}</strong></span>
        <ExpandChevron expanded={expanded} size={13} className="workspace-chevron" />
      </button>
      <button type="button" className="icon-button workspace-new" onClick={() => void appStore.newTask(workspaceKey(workspace.root))} aria-label={`在 ${name} 新建任务`}><Plus size={15} strokeWidth={1.7} aria-hidden="true" /></button>
      <button type="button" className="icon-button workspace-remove" onClick={onRemove} aria-label={`移除项目 ${name}`} aria-haspopup="dialog"><Minus size={16} strokeWidth={1.7} aria-hidden="true" /></button>
    </div>
  )
}

function SessionButton({
  session,
  selected,
  live,
  title,
  unread,
  onRename,
  onDelete,
}: {
  session: ThreadSummary
  selected: boolean
  live: AppState['liveSessions'][string] | undefined
  title: string
  unread: boolean
  onRename: () => void
  onDelete: () => void
}) {
  const [menuOpen, setMenuOpen] = useState(false)
  const anchor = useRef<HTMLButtonElement>(null)
  const status = sessionState(session, live)
  return (
    <div data-selected={selected} className={`session-row${selected ? ' is-selected' : ''}`} onContextMenu={event => { event.preventDefault(); setMenuOpen(true) }}>
      <button ref={anchor} type="button" className="session-main" aria-haspopup="menu" aria-expanded={menuOpen}
        onKeyDown={event => { if (event.key === 'ContextMenu' || (event.shiftKey && event.key === 'F10')) { event.preventDefault(); setMenuOpen(true) } }} onClick={() => {
        appStore.selectSession(session.threadId)
        if (window.matchMedia('(max-width: 760px)').matches) appStore.toggleSidebar()
      }}>
        <span className="session-label">
          <strong className={live && live.phase !== 'idle' ? 'activity-shimmer' : undefined}>{title}</strong>
          {session.status !== null && <time dateTime={session.updatedAt}>{relativeTime(session.updatedAt)}</time>}
        </span>
        <span className={`session-status status-${live && live.phase !== 'idle' ? 'running' : unread && !selected ? status.className : 'idle'}`} aria-hidden="true" />
        <span className="sr-only">{status.label}</span>
      </button>
      {menuOpen && <Menu anchor={anchor} label="任务菜单" onClose={() => setMenuOpen(false)} entries={[{ id: 'rename', label: '重命名', onSelect: onRename }, { id: 'delete', label: '删除任务', onSelect: onDelete }]} />}
    </div>
  )
}

function SidebarDialog({ state, onClose }: { state: PendingDialog; onClose: () => void }) {
  const [name, setName] = useState('')
  const { actionErrors, pendingActions } = useAppStore(['actionErrors', 'pendingActions'])
  const method = state.kind === 'rename' ? 'session.rename' : 'workspace.remove'
  const origin = state.kind === 'none' ? null : actionOrigin.inline(method, state.kind === 'rename' ? state.session.threadId : workspaceKey(state.workspace.root))
  const busy = origin !== null && pendingActions.has(pendingKey(method, origin))
  const close = () => { if (!busy) onClose() }
  const initialName = state.kind === 'rename' ? state.session.title ?? '' : ''
  useEffect(() => {
    setName(initialName)
    if (origin !== null) appStore.clearError(origin)
  }, [initialName, origin])
  const failure = origin === null ? undefined : actionErrors[origin]
  if (state.kind === 'none') return null
  if (state.kind === 'rename') {
    const submit = async () => {
      if (name.trim() === '') return
      const accepted = await appStore.renameSession(state.session.threadId, name.trim())
      if (accepted) onClose()
    }
    return (
      <Dialog open onClose={close} labelledBy="rename-title" className="confirm-modal">
        <header className="modal-header"><h2 id="rename-title">重命名任务</h2><button type="button" className="icon-button" disabled={busy} onClick={close} aria-label="关闭"><X size={18} /></button></header>
        <form className="confirm-body" onSubmit={(event) => { event.preventDefault(); void submit() }}>
          <label><span>名称</span><input data-autofocus disabled={busy} value={name} onChange={(event) => setName(event.target.value)} /></label>
          {failure && <p className="form-error" role="alert">{failure.message}</p>}
          <footer><button type="button" className="secondary-button" disabled={busy} onClick={close}>取消</button><button type="submit" className="primary-button" disabled={busy || name.trim() === ''}>保存</button></footer>
        </form>
      </Dialog>
    )
  }
  const title = '移除项目'
  const targetName = workspaceName(state.workspace)
  const confirm = async () => { if (await appStore.removeWorkspace(workspaceKey(state.workspace.root))) onClose() }
  return (
    <Dialog open onClose={close} labelledBy="confirm-title" className="confirm-modal">
      <header className="modal-header"><h2 id="confirm-title">{title}</h2><button type="button" className="icon-button" data-autofocus disabled={busy} onClick={close} aria-label="关闭"><X size={18} /></button></header>
      <div className="confirm-body">
        <p>{`移除“${targetName}”？已保存的任务历史会保留。`}</p>
        {failure && <p className="form-error" role="alert">{failure.message}</p>}
        <footer><button type="button" className="secondary-button" disabled={busy} onClick={close}>取消</button><button type="button" className="danger-button" disabled={busy} onClick={() => void confirm()}>{title}</button></footer>
      </div>
    </Dialog>
  )
}

function relativeTime(value: string): string {
  const then = Date.parse(value)
  if (!Number.isFinite(then)) return ''
  const seconds = Math.max(0, Math.floor((Date.now() - then) / 1_000))
  if (seconds < 60) return '刚刚'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}分钟`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}小时`
  const days = Math.floor(hours / 24)
  return days < 7 ? `${days}天` : new Intl.DateTimeFormat('zh-CN', { month: 'short', day: 'numeric' }).format(then)
}

function SessionList({ activeKey, children }: { activeKey: string | null; children: import('react').ReactNode }) {
  const container = useRef<HTMLDivElement>(null)
  return <div ref={container} className="session-list"><SelectionSlider container={container} activeKey={activeKey} />{children}</div>
}
