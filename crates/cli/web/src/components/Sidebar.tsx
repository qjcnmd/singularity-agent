import { useEffect, useState } from 'react'
import { connectionText, phaseText, turnStatusText } from '../copy'
import { useSelectionGuard } from '../interactions'
import { workbenchStore, type WorkbenchState } from '../store'
import type { ThreadSummary, Workspace } from '../protocol'
import { sessionDisplayTitle } from '../sessionTitle'
import { Dialog } from './Dialog'

type PendingDialog =
  | { kind: 'none' }
  | { kind: 'rename'; session: ThreadSummary }
  | { kind: 'archive'; session: ThreadSummary }
  | { kind: 'remove'; workspace: Workspace }

export function Sidebar({ state }: { state: WorkbenchState }) {
  const [dialog, setDialog] = useState<PendingDialog>({ kind: 'none' })
  const workspace = state.bootstrap?.workspaces.find((item) => item.workspaceId === state.selectedWorkspaceId) ?? null
  const sessions = workspace === null ? [] : state.bootstrap?.sessionsByWorkspace[workspace.workspaceId] ?? []
  const query = state.sessionSearch.trim().toLowerCase()
  const visibleSessions = query === ''
    ? sessions
    : sessions.filter((session) => `${sessionDisplayTitle(session, sessions)} ${session.threadId} ${session.model ?? ''}`.toLowerCase().includes(query))
  const createTask = async () => {
    if (await workbenchStore.createSession()) requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="任务说明"]')?.focus())
  }

  if (state.sidebarCollapsed) {
    return (
      <aside className="sidebar sidebar-rail" aria-label="项目与任务导航">
        <button type="button" className="brand-mark" onClick={() => workbenchStore.toggleSidebar()} aria-label="展开侧栏">S</button>
        <button type="button" className="rail-button" onClick={() => { void createTask() }} disabled={workspace === null} aria-label="新建任务">+</button>
        <div className="rail-spacer" />
        <ConnectionDot status={state.connection} />
      </aside>
    )
  }

  const createOrigin = workspace === null ? undefined : `workspace:${workspace.workspaceId}`
  return (
    <>
      <aside className="sidebar" aria-label="项目与任务导航">
        <header className="sidebar-brand">
          <div className="brand-lockup"><span className="brand-mark" aria-hidden="true">S</span><strong>Singularity</strong></div>
          <button type="button" className="icon-button" onClick={() => workbenchStore.toggleSidebar()} aria-label="收起侧栏">«</button>
        </header>

        <div className="new-task-wrap">
          <button
            type="button"
            className="new-task-button"
            disabled={workspace === null || workbenchStore.isPending('session.create', createOrigin)}
            onClick={() => { void createTask() }}
          >
            <span aria-hidden="true">＋</span>
            {workbenchStore.isPending('session.create', createOrigin) ? '正在新建…' : '新建任务'}
          </button>
        </div>

        <section className="sidebar-section workspace-section">
          <div className="section-heading">
            <span>项目</span>
            <button type="button" className="icon-button section-action" onClick={() => workbenchStore.openDirectoryPicker()} aria-label="添加项目">＋</button>
          </div>
          <div className="workspace-list">
            {state.bootstrap?.workspaces.map((item) => (
              <WorkspaceButton
                key={item.workspaceId}
                workspace={item}
                selected={item.workspaceId === state.selectedWorkspaceId}
                onRemove={() => setDialog({ kind: 'remove', workspace: item })}
              />
            ))}
            {(state.bootstrap?.workspaces.length ?? 0) === 0 && (
              <button type="button" className="workspace-empty" onClick={() => workbenchStore.openDirectoryPicker()}>
                <strong>添加第一个项目</strong><span>选择一个本地文件夹</span>
              </button>
            )}
          </div>
        </section>

        {workspace !== null && (
          <section className="sidebar-section session-section">
            <div className="section-heading"><span>任务</span><span className="section-count">{visibleSessions.length}</span></div>
            <label className="search-field">
              <span className="sr-only">搜索任务</span>
              <input type="search" value={state.sessionSearch} onChange={(event) => workbenchStore.setSessionSearch(event.target.value)} placeholder="搜索任务" />
            </label>
            <div className="session-list">
              {visibleSessions.map((session) => (
                <SessionButton
                  key={session.threadId}
                  session={session}
                  selected={session.threadId === state.selectedSessionId}
                  live={state.liveSessions[session.threadId]}
                  siblings={sessions}
                  onRename={() => setDialog({ kind: 'rename', session })}
                  onArchive={() => setDialog({ kind: 'archive', session })}
                />
              ))}
              {visibleSessions.length === 0 && <p className="sidebar-empty">{query === '' ? '还没有任务。' : '没有匹配的任务。'}</p>}
            </div>
          </section>
        )}

        <footer className="sidebar-footer">
          <button type="button" className="sidebar-settings" onClick={() => workbenchStore.setSettingsOpen(true)}>
            <span className="settings-mark" aria-hidden="true">⊙</span>
            <span><strong>设置</strong><small>模型供应商与连接</small></span>
          </button>
          <div className="host-status">
            <ConnectionStatus status={state.connection} />
            <span title="Agent 可以使用当前账户读取、编辑本机文件并运行命令。">完整本机权限</span>
          </div>
        </footer>
      </aside>
      <SidebarDialog state={dialog} onClose={() => setDialog({ kind: 'none' })} />
    </>
  )
}

function WorkspaceButton({ workspace, selected, onRemove }: { workspace: Workspace; selected: boolean; onRemove: () => void }) {
  const guard = useSelectionGuard()
  const removeGuard = useSelectionGuard()
  return (
    <div className={`workspace-row${selected ? ' is-selected' : ''}`}>
      <button
        type="button"
        className="workspace-button"
        {...guard(() => workbenchStore.selectWorkspace(workspace.workspaceId))}
        title={workspace.root}
      >
        <span className="workspace-glyph" aria-hidden="true">▱</span>
        <span><strong>{workspace.name}</strong><small>{workspace.root}</small></span>
      </button>
      <button type="button" className="icon-button danger workspace-remove" {...removeGuard(onRemove)} aria-label={`移除项目 ${workspace.name}`}>×</button>
    </div>
  )
}

function SessionButton({
  session,
  selected,
  live,
  siblings,
  onRename,
  onArchive,
}: {
  session: ThreadSummary
  selected: boolean
  live: WorkbenchState['liveSessions'][string] | undefined
  siblings: ThreadSummary[]
  onRename: () => void
  onArchive: () => void
}) {
  const mainGuard = useSelectionGuard()
  const renameGuard = useSelectionGuard()
  const archiveGuard = useSelectionGuard()
  const status = sessionState(session, live)
  return (
    <div className={`session-row${selected ? ' is-selected' : ''}`}>
      <button type="button" className="session-main" {...mainGuard(() => workbenchStore.selectSession(session.threadId))}>
        <span className={`session-status status-${status.className}`} aria-hidden="true" />
        <span>
          <strong>{sessionDisplayTitle(session, siblings)}</strong>
          <small>
            <span>{status.label}{session.turnCount > 0 && ` · ${session.turnCount} 回合`}</span>
            <time dateTime={session.updatedAt}>{relativeTime(session.updatedAt)}</time>
          </small>
        </span>
        <span className="sr-only">{status.label}</span>
      </button>
      <span className="session-actions">
        <button type="button" className="icon-button" {...renameGuard(onRename)} aria-label="重命名任务">✎</button>
        <button type="button" className="icon-button danger" {...archiveGuard(onArchive)} aria-label="归档任务">×</button>
      </span>
    </div>
  )
}

function SidebarDialog({ state, onClose }: { state: PendingDialog; onClose: () => void }) {
  const [name, setName] = useState('')
  const initialName = state.kind === 'rename' ? state.session.title ?? '' : ''
  useEffect(() => setName(initialName), [initialName, state.kind])
  if (state.kind === 'none') return null
  if (state.kind === 'rename') {
    const submit = async () => {
      if (name.trim() === '') return
      if (await workbenchStore.renameSession(state.session.threadId, name.trim())) onClose()
    }
    return (
      <Dialog open onClose={onClose} labelledBy="rename-title" className="confirm-modal">
        <header className="modal-header"><div><span className="eyebrow">任务</span><h2 id="rename-title">重命名任务</h2></div><button type="button" className="icon-button" onClick={onClose} aria-label="关闭">×</button></header>
        <form className="confirm-body" onSubmit={(event) => { event.preventDefault(); void submit() }}>
          <label><span>任务名称</span><input data-autofocus value={name} onChange={(event) => setName(event.target.value)} /></label>
          <footer><button type="button" className="secondary-button" onClick={onClose}>取消</button><button type="submit" className="primary-button" disabled={name.trim() === ''}>保存</button></footer>
        </form>
      </Dialog>
    )
  }
  const isArchive = state.kind === 'archive'
  const title = isArchive ? '归档任务' : '移除项目'
  const targetName = isArchive ? sessionDisplayTitle(state.session) : state.workspace.name
  const confirm = async () => {
    const accepted = isArchive
      ? await workbenchStore.archiveSession(state.session.threadId)
      : await workbenchStore.removeWorkspace(state.workspace.workspaceId)
    if (accepted) onClose()
  }
  return (
    <Dialog open onClose={onClose} labelledBy="confirm-title" className="confirm-modal">
      <header className="modal-header"><div><span className="eyebrow">确认操作</span><h2 id="confirm-title">{title}</h2></div><button type="button" className="icon-button" data-autofocus onClick={onClose} aria-label="关闭">×</button></header>
      <div className="confirm-body">
        <p>{isArchive ? `“${targetName}”的持久记录会保留，可在存储中恢复。` : `“${targetName}”只会从工作台列表移除，本机文件和已有会话不会被删除。`}</p>
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
    : { className: status, label: turnStatusText[status] }
}

function relativeTime(value: string): string {
  const then = Date.parse(value)
  if (!Number.isFinite(then)) return ''
  const seconds = Math.max(0, Math.floor((Date.now() - then) / 1_000))
  if (seconds < 60) return '刚刚'
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return days < 7 ? `${days} 天前` : new Intl.DateTimeFormat('zh-CN', { month: 'short', day: 'numeric' }).format(then)
}

function ConnectionStatus({ status }: { status: WorkbenchState['connection'] }) {
  if (status === 'ready') return <div className="connection-healthy"><ConnectionDot status={status} /> {connectionText.ready.title}</div>
  const copy = connectionText[status]
  return (
    <div className={`connection-card connection-${status}`}>
      <strong>{copy.title}</strong><span>{copy.detail}</span>
      {status === 'unavailable' && <button type="button" className="quiet-button" onClick={() => workbenchStore.retryConnection()}>重新连接</button>}
    </div>
  )
}

function ConnectionDot({ status }: { status: WorkbenchState['connection'] }) {
  return <span className={`connection-dot connection-${status}`} title={connectionText[status].title} role="img" aria-label={connectionText[status].title} />
}
