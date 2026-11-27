import { useEffect, useRef, useState } from 'react'
import { RotateCw, X } from 'lucide-react'
import { appStore, useAppStore, type AppState, type FileTab } from '../appStore'
import type { ChangedFile, SessionFileDiffResult, TextDocument } from '../protocol'
import { CodeTokens, useCodeTokens } from '../highlight'
import { MarkdownBody, PathBase } from '../markdown'
import { workspaceKey } from '../workspace'
import { EditDiff, FileChangeStats } from './EditDiff'
import { ExpandChevron } from './ExpandChevron'

type Loaded = { document?: TextDocument; changes?: ChangedFile[]; error?: string }

const extension = (path: string) => /\.([\w-]+)$/.exec(path)?.[1]?.toLowerCase() ?? ''
const isMarkdown = (path: string) => ['md', 'markdown'].includes(extension(path))

function diffRefreshKey(state: Pick<AppState, 'bootstrap' | 'liveSessions' | 'session'>, tab: FileTab | null): string {
  if (tab?.mode !== 'changes') return ''
  const wsKey = workspaceKey(tab.cwd)
  const sessions = state.bootstrap?.sessionsByWorkspace[wsKey] ?? []
  const sessionKeys = sessions.map(item => {
    const live = state.liveSessions[item.threadId]
    const tasks = live?.backgroundTasks.map(task => `${task.taskId}:${task.state.status}`).join(',') ?? ''
    return `${item.threadId}:${item.updatedAt}:${item.status ?? ''}:${live?.phase ?? ''}:${live?.summaryRevision ?? ''}:${tasks}`
  }).join('|')
  const current = state.session && workspaceKey(state.session.summary.cwd) === wsKey ? state.session : null
  return `${sessionKeys}#${current?.summary.threadId ?? ''}:${current?.facts.length ?? ''}:${current?.facts.at(-1)?.status ?? ''}`
}

function splitPathSegments(path: string): string[] {
  const normalized = path.replaceAll('\\', '/').replace(/\/+$/, '')
  return normalized.split('/').filter(Boolean)
}

function disambiguateWorkspaceLabel(cwd: string, allCwds: readonly string[]): string {
  const selfSegments = splitPathSegments(cwd)
  if (selfSegments.length === 0) return cwd
  const others = allCwds.filter(item => workspaceKey(item) !== workspaceKey(cwd)).map(splitPathSegments)
  for (let depth = 1; depth <= selfSegments.length; depth += 1) {
    const candidate = selfSegments.slice(-depth).join('/')
    const lower = candidate.toLowerCase()
    const collides = others.some(segments => segments.slice(-depth).join('/').toLowerCase() === lower)
    if (!collides) return candidate
  }
  return selfSegments.join('/')
}

export function FilePanel() {
  const state = useAppStore(['fileTabs', 'activeFileTab', 'bootstrap', 'liveSessions', 'session'], (previous, next) => {
    if (previous.fileTabs !== next.fileTabs || previous.activeFileTab !== next.activeFileTab) return false
    const tab = next.fileTabs.find(item => item.id === next.activeFileTab) ?? null
    return diffRefreshKey(previous, tab) === diffRefreshKey(next, tab)
  })
  const [documents, setDocuments] = useState<Record<string, Loaded>>({})
  const [sources, setSources] = useState<ReadonlySet<string>>(new Set())
  const [attempt, setAttempt] = useState(0)
  const active = state.fileTabs.find(tab => tab.id === state.activeFileTab) ?? null
  const refreshKey = diffRefreshKey(state, active)
  const loaded = active ? documents[active.id] : undefined
  const shown = loaded?.document
  const shownChanges = loaded?.changes

  useEffect(() => {
    if (!active) return
    let current = true
    const update = (patch: Loaded) => setDocuments(all => ({ ...all, [active.id]: { ...all[active.id], ...patch } }))
    if (active.mode === 'changes') {
      void appStore.transport.rpc('session.changedFiles', { workspaceRoot: active.cwd, includeDiffs: true }).then(
        changes => { if (current) update({ changes, error: undefined }) },
        error => { if (current) update({ changes: undefined, error: error instanceof Error ? error.message : String(error) }) },
      )
    } else {
      void appStore.transport.rpc('file.read', { cwd: active.cwd, path: active.path }).then(
        document => { if (current) update({ document, error: undefined }) },
        error => { if (current) update({ document: undefined, error: error instanceof Error ? error.message : String(error) }) },
      )
    }
    return () => { current = false }
  }, [active?.id, active?.opened, attempt, refreshKey])

  useEffect(() => {
    const openIds = new Set(state.fileTabs.map(tab => tab.id))
    setDocuments(all => Object.fromEntries(Object.entries(all).filter(([id]) => openIds.has(id))))
    setSources(all => {
      const next = new Set([...all].filter(id => openIds.has(id)))
      return next.size === all.size ? all : next
    })
  }, [state.fileTabs])

  if (!active) return null
  const isChanges = active.mode === 'changes'
  const showSource = sources.has(active.id)
  const toggleSource = () => setSources(all => { const next = new Set(all); if (next.has(active.id)) next.delete(active.id); else next.add(active.id); return next })
  const toolbarPath = shown?.path ?? active.path
  const ready = isChanges ? shownChanges : shown
  const totalAdded = shownChanges?.reduce((sum, file) => sum + file.added, 0) ?? 0
  const totalDeleted = shownChanges?.reduce((sum, file) => sum + file.deleted, 0) ?? 0
  const retryActive = () => {
    setDocuments(all => all[active.id] ? { ...all, [active.id]: { ...all[active.id], error: undefined } } : all)
    setAttempt(value => value + 1)
  }
  const changesCwds = state.fileTabs.filter(tab => tab.mode === 'changes').map(tab => tab.cwd)
  return <aside className="file-panel" aria-label="文件">
    <div className="file-tabs" role="tablist" aria-label="打开的文件">
      {state.fileTabs.map(tab => <TabButton key={tab.id} tab={tab} selected={tab.id === active.id} changesCwds={changesCwds} />)}
    </div>
    <div className="file-toolbar">
      {isChanges ? (
        <>
          <span className="changes-toolbar-title">
            {shownChanges ? `未提交改动 · ${shownChanges.length} 个文件` : '未提交改动'}
          </span>
          {(totalAdded > 0 || totalDeleted > 0) && (
            <span className="edit-counts">
              {totalAdded > 0 && <span className="edit-added-count">+{totalAdded}</span>}
              {totalDeleted > 0 && <span className="edit-deleted-count">-{totalDeleted}</span>}
            </span>
          )}
          <button type="button" className="icon-button small changes-refresh" aria-label="刷新改动文件" title="刷新" onClick={retryActive}>
            <RotateCw size={13} strokeWidth={1.8} aria-hidden="true" />
          </button>
        </>
      ) : (
        <>
          <span className="file-path" title={toolbarPath}><bdi>{toolbarPath}</bdi></span>
          {isMarkdown(active.path) && shown && <button type="button" className="quiet-button" onClick={toggleSource}>{showSource ? '预览' : '源码'}</button>}
        </>
      )}
    </div>
    <div className="file-body" role="tabpanel" aria-label={active.title}>
      {!ready && !loaded?.error && <p className="detail-empty" role="status">读取中…</p>}
      {loaded?.error && <p className="detail-empty form-error" role="alert">{loaded.error}
        <button type="button" className="quiet-button" onClick={retryActive}>重试</button></p>}
      {isChanges
        ? shownChanges && (
          <ChangedFilesView
            key={active.id}
            files={shownChanges}
            workspaceRoot={active.cwd}
            refreshToken={`${active.opened}:${attempt}:${refreshKey}`}
            focusPath={active.focusPath}
            focusNonce={active.opened}
          />
        )
        : shown && (isMarkdown(active.path) && !showSource
          ? <MarkdownDocumentView path={shown.path} content={shown.content} />
          : <CodeView code={shown.content} language={extension(active.path)} />)}
    </div>
  </aside>
}

function ChangedFilesView({ files, workspaceRoot, refreshToken, focusPath, focusNonce }: {
  files: ChangedFile[]
  workspaceRoot: string
  refreshToken: string
  focusPath?: string
  focusNonce: number
}) {
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set())
  const rowRefs = useRef(new Map<string, HTMLElement>())
  const appliedFocus = useRef<string | null>(null)

  useEffect(() => {
    const valid = new Set(files.map(file => file.path))
    setExpanded(all => {
      const next = new Set([...all].filter(path => valid.has(path)))
      return next.size === all.size ? all : next
    })
  }, [files])

  useEffect(() => {
    if (!focusPath) return
    const key = `${focusNonce}:${focusPath}`
    if (appliedFocus.current === key) return
    const target = focusPath.replaceAll('\\', '/').toLowerCase()
    const matched = files.find(file => file.path.replaceAll('\\', '/').toLowerCase() === target)
    if (!matched) return
    appliedFocus.current = key
    setExpanded(all => {
      if (all.has(matched.path)) return all
      const next = new Set(all)
      next.add(matched.path)
      return next
    })
    requestAnimationFrame(() => {
      rowRefs.current.get(matched.path)?.scrollIntoView({ block: 'start', behavior: 'smooth' })
    })
  }, [files, focusPath, focusNonce])

  const toggle = (path: string) => setExpanded(all => {
    const next = new Set(all)
    if (next.has(path)) next.delete(path)
    else next.add(path)
    return next
  })

  if (files.length === 0) return <p className="detail-empty">当前工作区没有未提交的改动。</p>
  return (
    <div className="changes-list">
      {files.map(file => {
        const isOpen = expanded.has(file.path)
        const normalized = file.path.replaceAll('\\', '/')
        const slash = normalized.lastIndexOf('/')
        const name = slash >= 0 ? normalized.slice(slash + 1) : normalized
        const dir = slash >= 0 ? normalized.slice(0, slash) : ''
        const statusLetter = file.status === 'added' ? 'A' : file.status === 'deleted' ? 'D' : 'M'
        return (
          <section
            key={file.path}
            ref={node => {
              if (node) rowRefs.current.set(file.path, node)
              else rowRefs.current.delete(file.path)
            }}
            className={`changes-item${isOpen ? ' is-open' : ''}`}
          >
            <button
              type="button"
              className="changes-row"
              aria-expanded={isOpen}
              title={file.path}
              onClick={() => toggle(file.path)}
            >
              <span className={`changes-status is-${file.status}`} aria-hidden="true">{statusLetter}</span>
              <span className="changes-path">
                <span className="changes-name">{name}</span>
                {dir && <span className="changes-dir">{dir}</span>}
              </span>
              <FileChangeStats file={file} />
              <ExpandChevron expanded={isOpen} size={12} />
            </button>
            {isOpen && (
              file.diff !== undefined
                ? <div className="changes-inline-diff"><EditDiff path={file.path} output={file.diff} bare /></div>
                : <LazyInlineFileDiff workspaceRoot={workspaceRoot} path={file.path} refreshToken={refreshToken} />
            )}
          </section>
        )
      })}
    </div>
  )
}

function LazyInlineFileDiff({ workspaceRoot, path, refreshToken }: {
  workspaceRoot: string
  path: string
  refreshToken: string
}) {
  const [retry, setRetry] = useState(0)
  const [state, setState] = useState<{ diff?: SessionFileDiffResult; error?: string }>({})

  useEffect(() => {
    let active = true
    setState(previous => previous.error ? { diff: previous.diff } : previous)
    void appStore.transport.rpc('session.fileDiff', { workspaceRoot, path }).then(
      diff => { if (active) setState({ diff }) },
      error => { if (active) setState({ error: error instanceof Error ? error.message : String(error) }) },
    )
    return () => { active = false }
  }, [workspaceRoot, path, refreshToken, retry])

  return (
    <div className="changes-inline-diff">
      {!state.diff && !state.error && <p className="changes-inline-status" role="status">读取差异中…</p>}
      {state.error && (
        <p className="changes-inline-status form-error" role="alert">
          {state.error}
          <button type="button" className="quiet-button" onClick={() => setRetry(value => value + 1)}>
            重试
          </button>
        </p>
      )}
      {state.diff && <EditDiff path={state.diff.path} output={state.diff.output} bare />}
    </div>
  )
}

function TabButton({ tab, selected, changesCwds }: { tab: FileTab; selected: boolean; changesCwds: readonly string[] }) {
  const label = tab.mode === 'changes' && changesCwds.length > 1
    ? `${tab.title} · ${disambiguateWorkspaceLabel(tab.cwd, changesCwds)}`
    : tab.title
  const title = tab.mode === 'changes' ? `${tab.cwd} · 工作区改动文件` : tab.path
  return <div className="file-tab" data-selected={selected}>
    <button type="button" role="tab" aria-selected={selected} title={title} onClick={() => appStore.selectFile(tab.id)}
      onAuxClick={event => { if (event.button === 1) appStore.closeFile(tab.id) }}>{label}</button>
    <button type="button" className="file-tab-close" aria-label={`关闭 ${label}`} onClick={() => appStore.closeFile(tab.id)}><X size={13} /></button>
  </div>
}

const maxHighlightedLength = 200_000

export function MarkdownDocumentView({ path, content }: { path: string; content: string }) {
  return <PathBase.Provider value={directoryOf(path)}>
    <div className="file-markdown timeline-body message-body"><MarkdownBody text={frontmatterAsCode(content)} /></div>
  </PathBase.Provider>
}

export function CodeView({ code, language }: { code: string; language: string }) {
  const normalized = code.replace(/\r\n/g, '\n')
  const tokens = useCodeTokens(normalized, normalized.length > maxHighlightedLength ? 'text' : language)
  const lines = normalized.split('\n')
  return <div className="file-code">{lines.map((line, row) => <div key={row} className="tool-line">
    <span className="tool-line-number">{row + 1}</span>
    <span>{tokens?.[row] ? <CodeTokens tokens={tokens[row]} /> : line}</span>
  </div>)}</div>
}

function frontmatterAsCode(content: string): string {
  return content.replace(/^---\r?\n([\s\S]*?)\r?\n---\r?\n/, (_, yaml: string) => '```yaml\n' + yaml + '\n```\n\n')
}

function directoryOf(path: string): string {
  return path.replace(/[\\/][^\\/]*$/, '') || path
}
