import { useEffect, useState } from 'react'
import { X } from 'lucide-react'
import { appStore, useAppStore, type AppState, type FileTab } from '../appStore'
import type { SessionFileDiffResult, TextDocument } from '../protocol'
import { CodeTokens, useCodeTokens } from '../highlight'
import { MarkdownBody, PathBase } from '../markdown'
import { EditDiff } from './EditDiff'

type Loaded = { document?: TextDocument; diff?: SessionFileDiffResult; error?: string }

const extension = (path: string) => /\.([\w-]+)$/.exec(path)?.[1]?.toLowerCase() ?? ''
const isMarkdown = (path: string) => ['md', 'markdown'].includes(extension(path))

function diffRefreshKey(state: Pick<AppState, 'liveSessions' | 'session'>, tab: FileTab | null): string {
  if (!tab || tab.mode !== 'diff' || !tab.sessionId) return ''
  const live = state.liveSessions[tab.sessionId]
  const current = state.session?.summary.threadId === tab.sessionId ? state.session : null
  return `${live?.phase ?? ''}:${live?.summaryRevision ?? ''}:${current?.facts.length ?? ''}:${current?.facts.at(-1)?.status ?? ''}`
}

export function FilePanel() {
  const state = useAppStore(['fileTabs', 'activeFileTab', 'liveSessions', 'session'], (previous, next) => {
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
  const shownDiff = loaded?.diff

  useEffect(() => {
    if (!active) return
    let current = true
    const update = (patch: Loaded) => setDocuments(all => ({ ...all, [active.id]: { ...all[active.id], ...patch } }))
    if (active.mode === 'diff' && active.sessionId) {
      void appStore.transport.rpc('session.fileDiff', { sessionId: active.sessionId, path: active.path }).then(
        diff => { if (current) update({ diff, error: undefined }) },
        error => { if (current) update({ error: error instanceof Error ? error.message : String(error) }) },
      )
    } else {
      void appStore.transport.rpc('file.read', { cwd: active.cwd, path: active.path }).then(
        document => { if (current) update({ document, error: undefined }) },
        error => { if (current) update({ error: error instanceof Error ? error.message : String(error) }) },
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
  const isDiff = active.mode === 'diff'
  const showSource = sources.has(active.id)
  const toggleSource = () => setSources(all => { const next = new Set(all); if (next.has(active.id)) next.delete(active.id); else next.add(active.id); return next })
  const toolbarPath = isDiff ? shownDiff?.path ?? active.path : shown?.path ?? active.path
  const ready = isDiff ? shownDiff : shown
  return <aside className="file-panel" aria-label="文件">
    <div className="file-tabs" role="tablist" aria-label="打开的文件">
      {state.fileTabs.map(tab => <TabButton key={tab.id} tab={tab} selected={tab.id === active.id} />)}
    </div>
    <div className="file-toolbar">
      <span className="file-path" title={toolbarPath}>{toolbarPath}</span>
      {!isDiff && isMarkdown(active.path) && shown && <button type="button" className="quiet-button" onClick={toggleSource}>{showSource ? '预览' : '源码'}</button>}
    </div>
    <div className="file-body" role="tabpanel" aria-label={active.title}>
      {!ready && !loaded?.error && <p className="detail-empty" role="status">读取中…</p>}
      {loaded?.error && <p className="detail-empty form-error" role="alert">{loaded.error}
        <button type="button" className="quiet-button" onClick={() => setAttempt(value => value + 1)}>重试</button></p>}
      {isDiff
        ? shownDiff && <EditDiff path={shownDiff.path} output={shownDiff.output} />
        : shown && (isMarkdown(active.path) && !showSource
          ? <PathBase.Provider value={directoryOf(shown.path)}><div className="file-markdown timeline-body message-body"><MarkdownBody text={frontmatterAsCode(shown.content)} /></div></PathBase.Provider>
          : <CodeView code={shown.content} language={extension(active.path)} />)}
    </div>
  </aside>
}

function TabButton({ tab, selected }: { tab: FileTab; selected: boolean }) {
  return <div className="file-tab" data-selected={selected}>
    <button type="button" role="tab" aria-selected={selected} title={tab.mode === 'diff' ? `${tab.path}（差异）` : tab.path} onClick={() => appStore.selectFile(tab.id)}
      onAuxClick={event => { if (event.button === 1) appStore.closeFile(tab.id) }}>{tab.title}</button>
    <button type="button" className="file-tab-close" aria-label={`关闭 ${tab.title}`} onClick={() => appStore.closeFile(tab.id)}><X size={13} /></button>
  </div>
}

const maxHighlightedLength = 200_000

function CodeView({ code, language }: { code: string; language: string }) {
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
