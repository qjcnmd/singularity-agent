import { useEffect, useState } from 'react'
import { X } from 'lucide-react'
import { appStore, useAppStore, type FileTab } from '../appStore'
import type { TextDocument } from '../protocol'
import { CodeTokens, useCodeTokens } from '../highlight'
import { MarkdownBody, PathBase } from '../markdown'

/** 重新读取期间和读取失败时都保留上一次读到的内容，不闪“读取中”。 */
type Loaded = { document?: TextDocument; error?: string }

const extension = (path: string) => /\.([\w-]+)$/.exec(path)?.[1]?.toLowerCase() ?? ''
const isMarkdown = (path: string) => ['md', 'markdown'].includes(extension(path))

/** 右侧文件栏：每个文件一个标签页，只读；Markdown 默认排版显示，可切到源码。 */
export function FilePanel() {
  const state = useAppStore(['fileTabs', 'activeFileTab'])
  const [documents, setDocuments] = useState<Record<string, Loaded>>({})
  const [sources, setSources] = useState<ReadonlySet<string>>(new Set())
  const [attempt, setAttempt] = useState(0)
  const active = state.fileTabs.find(tab => tab.id === state.activeFileTab) ?? null
  const loaded = active ? documents[active.id] : undefined
  const shown = loaded?.document

  // 文件可能已被 Agent 或编辑器改过：切到标签、再次点开同一文件或点重试时都重新读取。
  useEffect(() => {
    if (!active) return
    let current = true
    const update = (patch: Loaded) => setDocuments(all => ({ ...all, [active.id]: { ...all[active.id], ...patch } }))
    void appStore.transport.rpc('file.read', { cwd: active.cwd, path: active.path }).then(
      document => { if (current) update({ document, error: undefined }) },
      error => { if (current) update({ error: error instanceof Error ? error.message : String(error) }) },
    )
    return () => { current = false }
  }, [active?.id, active?.opened, attempt])

  // 关掉的标签不再占着内容。
  useEffect(() => {
    setDocuments(all => Object.fromEntries(Object.entries(all).filter(([id]) => state.fileTabs.some(tab => tab.id === id))))
  }, [state.fileTabs])

  if (!active) return null
  const showSource = sources.has(active.id)
  const toggleSource = () => setSources(all => { const next = new Set(all); if (next.has(active.id)) next.delete(active.id); else next.add(active.id); return next })
  return <aside className="file-panel" aria-label="文件">
    <div className="file-tabs" role="tablist" aria-label="打开的文件">
      {state.fileTabs.map(tab => <TabButton key={tab.id} tab={tab} selected={tab.id === active.id} />)}
    </div>
    <div className="file-toolbar">
      <span className="file-path" title={shown?.path ?? active.path}>{shown?.path ?? active.path}</span>
      {isMarkdown(active.path) && shown && <button type="button" className="quiet-button" onClick={toggleSource}>{showSource ? '预览' : '源码'}</button>}
    </div>
    <div className="file-body" role="tabpanel" aria-label={active.title}>
      {!shown && !loaded?.error && <p className="detail-empty" role="status">读取中…</p>}
      {loaded?.error && <p className="detail-empty form-error" role="alert">{loaded.error}
        <button type="button" className="quiet-button" onClick={() => setAttempt(value => value + 1)}>重试</button></p>}
      {shown && (isMarkdown(active.path) && !showSource
        ? <PathBase.Provider value={directoryOf(shown.path)}><div className="file-markdown timeline-body message-body"><MarkdownBody text={frontmatterAsCode(shown.content)} /></div></PathBase.Provider>
        : <CodeView code={shown.content} language={extension(active.path)} />)}
    </div>
  </aside>
}

function TabButton({ tab, selected }: { tab: FileTab; selected: boolean }) {
  return <div className="file-tab" data-selected={selected}>
    <button type="button" role="tab" aria-selected={selected} title={tab.path} onClick={() => appStore.selectFile(tab.id)}
      onAuxClick={event => { if (event.button === 1) appStore.closeFile(tab.id) }}>{tab.title}</button>
    <button type="button" className="file-tab-close" aria-label={`关闭 ${tab.title}`} onClick={() => appStore.closeFile(tab.id)}><X size={13} /></button>
  </div>
}

const maxHighlightedLength = 200_000

function CodeView({ code, language }: { code: string; language: string }) {
  // 很大的文件逐行高亮会卡住界面，只显示纯文本。
  const tokens = useCodeTokens(code, code.length > maxHighlightedLength ? 'text' : language)
  const lines = code.split('\n')
  return <div className="file-code">{lines.map((line, row) => <div key={row} className="tool-line">
    <span className="tool-line-number">{row + 1}</span>
    <span>{tokens?.[row] ? <CodeTokens tokens={tokens[row]} /> : line}</span>
  </div>)}</div>
}

/** SKILL.md 这类文件开头的 YAML 头按 Markdown 排版会挤成一段文字，改成代码块显示。 */
function frontmatterAsCode(content: string): string {
  return content.replace(/^---\r?\n([\s\S]*?)\r?\n---\r?\n/, (_, yaml: string) => '```yaml\n' + yaml + '\n```\n\n')
}

function directoryOf(path: string): string {
  return path.replace(/[\\/][^\\/]*$/, '') || path
}
