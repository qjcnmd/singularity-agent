import { SidebarToggle } from './SidebarToggle'
import { memo, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { phaseText, turnStatusText } from '../copy'
import { motion, useReducedMotion } from 'motion/react'
import { useSelectionGuard } from '../interactions'
import { workbenchStore, useWorkbenchStore, type WorkbenchState } from '../store'
import type { ThreadSummary, Workspace } from '../protocol'
import { sessionDisplayTitle } from '../sessionTitle'
import { Dialog } from './Dialog'
import { Menu } from './Menu'
import { Disclosure } from './Disclosure'
import { defaultWorkspaceAppearance, WorkspaceAppearancePicker, WorkspaceIcon } from './WorkspaceAppearancePicker'
import type { WorkspaceAppearance } from '../store'

type PendingDialog =
  | { kind: 'none' }
  | { kind: 'rename'; session: ThreadSummary }
  | { kind: 'workspace-rename'; workspace: Workspace }
  | { kind: 'remove'; workspace: Workspace }

export const Sidebar = memo(SidebarView)

function SidebarView() {
  const state = useWorkbenchStore(['bootstrap', 'liveSessions', 'selectedSessionId', 'selectedWorkspaceId', 'sidebarCollapsed', 'sidebarView', 'unreadSessions', 'workspaceAppearance', 'pendingActions', 'actionErrors'])
  const reducedMotion = useReducedMotion()
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
    workbenchStore.toggleSidebar()
  }
  useLayoutEffect(() => {
    const target = sidebarFocus.current
    if (!target) return
    const frame = requestAnimationFrame(focusSidebar)
    return () => cancelAnimationFrame(frame)
  }, [state.sidebarCollapsed])
  const [dialog, setDialog] = useState<PendingDialog>({ kind: 'none' })
  const collapsed = new Set(state.sidebarView.collapsed)
  const [viewOpen, setViewOpen] = useState(false)
  const viewAnchor = useRef<HTMLButtonElement>(null)
  const [showAll, setShowAll] = useState<Set<string>>(new Set())
  const allSessions = Object.values(state.bootstrap?.sessionsByWorkspace ?? {}).flat()
  const sorted = (sessions: ThreadSummary[]) => [...sessions].sort((a, b) => {
    if (state.sidebarView.order === 'updated') return b.updatedAt.localeCompare(a.updatedAt)
    const position = (id: string) => { const index = state.sidebarView.sessionOrder.indexOf(id); return index < 0 ? Number.MAX_SAFE_INTEGER : index }
    return position(a.threadId) - position(b.threadId)
  })
  const sessionRow = (session: ThreadSummary, siblings: ThreadSummary[], index = 0, expanded = true) => <motion.div key={session.threadId} initial={{ opacity: reducedMotion ? 1 : 0, y: reducedMotion ? 0 : -12 }} animate={{ opacity: expanded ? 1 : 0, y: reducedMotion || expanded ? 0 : -12 }} transition={{ duration: reducedMotion ? 0 : 0.24, delay: reducedMotion || !expanded ? 0 : index * 0.05, ease: 'easeOut' }} draggable={state.sidebarView.order === 'manual'}
    onDragStartCapture={event => event.dataTransfer.setData('text/plain', session.threadId)}
    onDragOver={event => { if (state.sidebarView.order === 'manual') event.preventDefault() }}
    onDrop={event => {
      event.preventDefault()
      const source = event.dataTransfer.getData('text/plain')
      if (source === session.threadId || !allSessions.some(item => item.threadId === source)) return
      const order = sorted(allSessions).map(item => item.threadId).filter(id => id !== source)
      order.splice(order.indexOf(session.threadId), 0, source)
      workbenchStore.setSidebarView({ sessionOrder: order })
    }}><SessionButton session={session} siblings={siblings} selected={session.threadId === state.selectedSessionId} live={state.liveSessions[session.threadId]}
      unread={state.unreadSessions.has(session.threadId)} onRename={() => setDialog({ kind: 'rename', session })} onArchive={() => { void workbenchStore.archiveSession(session.threadId) }} /></motion.div>
  return (
    <>
            <aside ref={sidebar} className={`sidebar-shell${state.sidebarCollapsed ? ' is-collapsed' : ''}`} aria-label="项目与任务导航" onTransitionEnd={focusSidebar}>
      <div className="sidebar sidebar-rail" inert={!state.sidebarCollapsed} aria-hidden={!state.sidebarCollapsed}>
        <SidebarToggle side="left" expanded={!state.sidebarCollapsed} controls="left-sidebar" onClick={toggleSidebar} />

      </div>
      <div id="left-sidebar" className="sidebar sidebar-expanded" inert={state.sidebarCollapsed} aria-hidden={state.sidebarCollapsed}>
        <header className="sidebar-brand">
          <div className="brand-lockup"><strong>Singularity</strong></div>
          <SidebarToggle side="left" expanded={!state.sidebarCollapsed} controls="left-sidebar" onClick={toggleSidebar} />
        </header>

        <section className="sidebar-section workspace-section">
          <div className="section-heading">
            <span>项目</span>
            <button ref={viewAnchor} type="button" className="icon-button" aria-label="视图选项" aria-haspopup="menu" aria-expanded={viewOpen} onClick={() => setViewOpen(value => !value)}><SidebarIcon name="view" /></button>
            <button type="button" className="icon-button section-action" onClick={() => workbenchStore.openDirectoryPicker()} aria-label="添加项目"><SidebarIcon name="folder" /></button>
          </div>
          {viewOpen && <Menu anchor={viewAnchor} label="视图选项" onClose={() => setViewOpen(false)} entries={[
            { id: 'workspace', label: '按项目分组', checked: state.sidebarView.grouping === 'workspace' },
            { id: 'flat', label: '全部任务', checked: state.sidebarView.grouping === 'flat' },
            { id: 'manual', label: '手动排序', checked: state.sidebarView.order === 'manual', divider: true },
            { id: 'updated', label: '最近更新', checked: state.sidebarView.order === 'updated' },
          ]} onPick={id => { if (id === 'workspace' || id === 'flat') workbenchStore.setSidebarView({ grouping: id }); else if (id === 'manual' || id === 'updated') workbenchStore.setSidebarView({ order: id }) }} />}
          <div className="workspace-list">
            {(state.sidebarView.grouping === 'flat') ? sorted(allSessions).map((session, index) => sessionRow(session, allSessions, index)) : state.bootstrap?.workspaces.map((item) => {
              const sessions = sorted(state.bootstrap?.sessionsByWorkspace[item.workspaceId] ?? [])
              const expanded = !collapsed.has(item.workspaceId)
              const visible = showAll.has(item.workspaceId) ? sessions : sessions.slice(0, 5)
              return <section key={item.workspaceId} className="workspace-tree">
                <WorkspaceButton workspace={item} selected={item.workspaceId === state.selectedWorkspaceId} expanded={expanded} appearance={state.workspaceAppearance[item.workspaceId] ?? defaultWorkspaceAppearance}
                  onToggle={() => { const next = new Set(collapsed); if (next.has(item.workspaceId)) { next.delete(item.workspaceId); setShowAll(previous => { const reset = new Set(previous); reset.delete(item.workspaceId); return reset }) } else next.add(item.workspaceId); workbenchStore.setSidebarView({ collapsed: [...next] }) }}
                  onRename={() => setDialog({ kind: 'workspace-rename', workspace: item })} onRemove={() => setDialog({ kind: 'remove', workspace: item })} />
                <Disclosure open={expanded}><div className="session-list">
                  {visible.map((session, index) => sessionRow(session, sessions, index, expanded))}
                  {sessions.length > 5 && <button type="button" className="quiet-button" onClick={() => setShowAll((previous) => { const next = new Set(previous); if (next.has(item.workspaceId)) next.delete(item.workspaceId); else next.add(item.workspaceId); return next })}>{showAll.has(item.workspaceId) ? '收起' : `显示更多 (${sessions.length - 5})`}</button>}
                  {sessions.length === 0 && <button type="button" className="quiet-button" onClick={() => void workbenchStore.createSession(item.workspaceId)}>新建任务</button>}
                </div></Disclosure>
              </section>
            })}
            {(state.bootstrap?.workspaces.length ?? 0) === 0 && <button type="button" className="workspace-empty" onClick={() => workbenchStore.openDirectoryPicker()}><strong>添加第一个项目</strong><span>选择一个本地文件夹</span></button>}
          </div>
        </section>


      </div></aside>
      <SidebarDialog state={dialog} onClose={() => setDialog({ kind: 'none' })} />
    </>
  )
}

function WorkspaceButton({ workspace, selected, expanded, appearance, onToggle, onRename, onRemove }: { workspace: Workspace; selected: boolean; expanded: boolean; appearance: WorkspaceAppearance; onToggle: () => void; onRename: () => void; onRemove: () => void }) {
  const guard = useSelectionGuard()
  const [menuOpen, setMenuOpen] = useState(false)
  const [appearanceOpen, setAppearanceOpen] = useState(false)
  const iconAnchor = useRef<HTMLButtonElement>(null)
  const anchor = useRef<HTMLButtonElement>(null)
  return (
    <div className={`workspace-row${selected ? ' is-selected' : ''}`} onContextMenu={event => { event.preventDefault(); setMenuOpen(true) }}>
      <button ref={iconAnchor} type="button" className="workspace-icon-button" aria-label={`更改 ${workspace.name} 的图标`} title="更改项目图标" aria-haspopup="dialog" aria-expanded={appearanceOpen} onClick={() => setAppearanceOpen(value => !value)}><WorkspaceIcon appearance={appearance} /></button>
      <button
        type="button"
        className="workspace-button"
        {...guard(onToggle)}
        aria-expanded={expanded}
        title={workspace.root}
      >
        <span className="workspace-name"><strong>{workspace.name}</strong></span>
      </button>
      <button type="button" className="icon-button workspace-new" onClick={() => void workbenchStore.createSession(workspace.workspaceId)} aria-label={`在 ${workspace.name} 新建任务`}>＋</button>
      <button ref={anchor} type="button" className="icon-button workspace-remove" onClick={() => setMenuOpen(value => !value)} aria-label={`项目菜单 ${workspace.name}`} aria-haspopup="menu" aria-expanded={menuOpen}>⋯</button>
      {menuOpen && <Menu anchor={anchor} label="项目菜单" onClose={() => setMenuOpen(false)} entries={[{id: 'appearance', label:'更改图标'}, {id: 'rename', label:'重命名'}, {id:'remove',label:'移除项目',danger:true}]} onPick={id => { if (id === 'appearance') setAppearanceOpen(true); else if (id === 'rename') onRename(); else onRemove() }} />}
      {appearanceOpen && <WorkspaceAppearancePicker anchor={iconAnchor} appearance={appearance} onChange={value => workbenchStore.setWorkspaceAppearance(workspace.workspaceId, value)} onClose={() => { setAppearanceOpen(false); requestAnimationFrame(() => iconAnchor.current?.focus()) }} />}
    </div>
  )
}

function SessionButton({
  session,
  selected,
  live,
  siblings,
  unread,
  onRename,
  onArchive,
}: {
  session: ThreadSummary
  selected: boolean
  live: WorkbenchState['liveSessions'][string] | undefined
  siblings: ThreadSummary[]
  unread: boolean
  onRename: () => void
  onArchive: () => void
}) {
  const mainGuard = useSelectionGuard()
  const [menuOpen, setMenuOpen] = useState(false)
  const anchor = useRef<HTMLButtonElement>(null)
  const status = sessionState(session, live)
  return (
    <div className={`session-row${selected ? ' is-selected' : ''}`} onContextMenu={event => { event.preventDefault(); setMenuOpen(true) }}>
      <button type="button" className="session-main" title={`${sessionDisplayTitle(session, siblings)}\n${status.label}`} {...mainGuard(() => {
        workbenchStore.selectSession(session.threadId)
        if (window.matchMedia('(max-width: 760px)').matches) workbenchStore.toggleSidebar()
      })}>
        <span className={`session-status status-${unread && !selected ? status.className : 'idle'}`} aria-hidden="true" />
        <span className="session-label">
          <strong className={live && live.phase !== 'idle' ? 'activity-shimmer' : undefined}>{sessionDisplayTitle(session, siblings)}</strong>
          {session.turnCount > 0 && <time dateTime={session.updatedAt}>{relativeTime(session.updatedAt)}</time>}
        </span>
        <span className="sr-only">{status.label}</span>
      </button>
      <span className="session-actions"><button ref={anchor} type="button" className="icon-button" aria-label="任务菜单" aria-haspopup="menu" aria-expanded={menuOpen} onClick={() => setMenuOpen(value => !value)}>⋯</button></span>
      {menuOpen && <Menu anchor={anchor} label="任务菜单" onClose={() => setMenuOpen(false)} entries={[{id:'rename',label:'重命名'}, {id:'archive',label:'归档任务'}]} onPick={id => { if (id === 'rename') onRename(); else onArchive() }} />}
    </div>
  )
}

function SidebarDialog({ state, onClose }: { state: PendingDialog; onClose: () => void }) {
  const [name, setName] = useState('')
  const initialName = state.kind === 'rename' ? state.session.title ?? '' : state.kind === 'workspace-rename' ? state.workspace.name : ''
  useEffect(() => setName(initialName), [initialName, state.kind])
  if (state.kind === 'none') return null
  if (state.kind === 'rename' || state.kind === 'workspace-rename') {
    const title = state.kind === 'rename' ? '重命名任务' : '重命名项目'
    const origin = state.kind === 'rename' ? `session:${state.session.threadId}` : `workspace:${state.workspace.workspaceId}`
    const failure = workbenchStore.getSnapshot().actionErrors[origin]
    const submit = async () => {
      if (name.trim() === '') return
      const accepted = state.kind === 'rename' ? await workbenchStore.renameSession(state.session.threadId, name.trim()) : await workbenchStore.renameWorkspace(state.workspace.workspaceId, name.trim())
      if (accepted) onClose()
    }
    return (
      <Dialog open onClose={onClose} labelledBy="rename-title" className="confirm-modal">
        <header className="modal-header"><div><span className="eyebrow">任务</span><h2 id="rename-title">{title}</h2></div><button type="button" className="icon-button" onClick={onClose} aria-label="关闭">×</button></header>
        <form className="confirm-body" onSubmit={(event) => { event.preventDefault(); void submit() }}>
          <label><span>名称</span><input data-autofocus value={name} onChange={(event) => setName(event.target.value)} /></label>
          {failure && <p className="form-error" role="alert">{failure.message}</p>}
          <footer><button type="button" className="secondary-button" onClick={onClose}>取消</button><button type="submit" className="primary-button" disabled={name.trim() === ''}>保存</button></footer>
        </form>
      </Dialog>
    )
  }
  const title = '移除项目'
  const targetName = state.workspace.name
  const failure = workbenchStore.getSnapshot().actionErrors[`workspace:${state.workspace.workspaceId}`]
  const confirm = async () => { if (await workbenchStore.removeWorkspace(state.workspace.workspaceId)) onClose() }
  return (
    <Dialog open onClose={onClose} labelledBy="confirm-title" className="confirm-modal">
      <header className="modal-header"><div><span className="eyebrow">确认操作</span><h2 id="confirm-title">{title}</h2></div><button type="button" className="icon-button" data-autofocus onClick={onClose} aria-label="关闭">×</button></header>
      <div className="confirm-body">
        <p>{`“${targetName}”只会从工作台列表移除，本机文件和已有任务不会被删除。`}</p>
        {failure && <p className="form-error" role="alert">{failure.message} {failure.recovery}</p>}
        <footer><button type="button" className="secondary-button" onClick={onClose}>取消</button><button type="button" className="danger-button" onClick={() => void confirm()}>{title}</button></footer>
      </div>
    </Dialog>
  )
}

function sessionState(session: ThreadSummary, live: WorkbenchState['liveSessions'][string] | undefined) {
  if (live !== undefined && live.phase !== 'idle') return { className: live.phase, label: phaseText[live.phase] }
  const status = live?.terminal?.status ?? session.status
  return status === null || status === undefined
    ? { className: 'idle', label: '就绪' }
    : { className: status === 'interrupted' && session.manuallyStopped ? 'stopped' : status, label: status === 'interrupted' && !session.manuallyStopped ? '任务异常中断' : turnStatusText[status] }
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

function SidebarIcon({ name }: { name: 'folder' | 'view' }) {
  const paths = {
    folder: 'M20 12v8H3V5h6l3 3h3M19 3v6M16 6h6',
    view: 'M3 6h7m4 0h7M3 12h3m4 0h11M3 18h11m4 0h3M10 4v4M6 10v4M14 16v4',
  }
  return <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d={paths[name]} /></svg>
}
