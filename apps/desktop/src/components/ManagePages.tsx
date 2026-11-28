import { useEffect, useState, type ReactNode } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { appStore, useAppStore } from '../appStore'
import type { SkillMetadata, TextDocument } from '../protocol'
import { workspaceName } from '../copy'
import { workspaceKey } from '../workspace'
import { McpSettings } from './McpSettings'
import { CodeView, MarkdownDocumentView } from './FilePanel'
import { smooth } from '../motion'
import { ArrowLeft, CircleCheck, ChevronRight, RefreshCw, Search } from 'lucide-react'

export function PageFrame({ title, actions, children }: { title?: string; actions?: ReactNode; children: ReactNode }) {
  return <div className="manage-page">
    {title && <header className="manage-header">
      <div className="manage-heading">
        <h3>{title}</h3>
      </div>
      {actions && <div className="manage-actions">{actions}</div>}
    </header>}
    {children}
  </div>
}

export function InstructionsPage() {
  const state = useAppStore(['bootstrap'])
  const [target, setTarget] = useState<string | null>(null)
  const [documents, setDocuments] = useState<Record<string, TextDocument>>({})
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [showSaved, setShowSaved] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  const workspaces = state.bootstrap?.workspaces
  const targetExists = target === null
    || workspaces === undefined
    || workspaces.some(workspace => workspaceKey(workspace.root) === target)
  const effectiveTarget = targetExists ? target : null
  useEffect(() => {
    if (!targetExists) setTarget(null)
  }, [targetExists])
  const reduced = useReducedMotion()
  const key = effectiveTarget ?? 'global'
  const saved = documents[key]
  const text = drafts[key] ?? saved?.content ?? ''
  const dirty = saved !== undefined && text !== saved.content
  useEffect(() => {
    let active = true
    setLoading(true)
    setError(null)
    void appStore.transport.rpc('instructions.read', { workspaceRoot: effectiveTarget }).then(document => {
      if (active) setDocuments(current => ({ ...current, [key]: document }))
    }).catch(error => { if (active) setError(error instanceof Error ? error.message : String(error)) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [effectiveTarget, key, reload])
  useEffect(() => {
    if (!showSaved) return
    const timeout = window.setTimeout(() => setShowSaved(false), 2400)
    return () => window.clearTimeout(timeout)
  }, [showSaved])
  async function save() {
    setSaving(true)
    setShowSaved(false)
    setError(null)
    try {
      const document = await appStore.transport.rpc('instructions.save', { workspaceRoot: effectiveTarget, content: text })
      setDocuments(current => ({ ...current, [key]: document }))
      setDrafts(current => { const next = { ...current }; delete next[key]; return next })
      setShowSaved(true)
      appStore.invalidateContextPreview()
    } catch (error) { setError(error instanceof Error ? error.message : String(error)) }
    finally { setSaving(false) }
  }
  return <PageFrame title="自定义指令">
    <div className="instructions-layout">
      <nav className="instruction-targets" aria-label="指令范围" role="tablist">
        <button type="button" role="tab" disabled={saving} aria-selected={effectiveTarget === null} onClick={() => setTarget(null)}>全局</button>
        {state.bootstrap?.workspaces.map(workspace => {
          const root = workspaceKey(workspace.root)
          return <button key={root} type="button" role="tab" disabled={saving} aria-selected={effectiveTarget === root} onClick={() => setTarget(root)}>
            {workspaceName(workspace)}
          </button>
        })}
      </nav>
      <motion.section key={key} className="instruction-editor" initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ duration: reduced ? 0 : .18 }} aria-busy={loading || saving}>
        <div className="editor-card-bar">
          <span>文件</span>
          <span className="editor-path">{saved?.path ?? (loading ? '读取中…' : '—')}</span>
        </div>
        {error && <div className="form-error" role="alert">{error}{!saved && <button type="button" className="quiet-button" onClick={() => setReload(value => value + 1)}>重新读取</button>}</div>}
        <textarea aria-label="自定义指令内容" className="input" value={text} spellCheck={false} disabled={loading || saving || !saved}
          onChange={event => setDrafts(current => ({ ...current, [key]: event.target.value }))} />
        <footer>
          <span className={`editor-status${showSaved ? ' is-saved' : ''}`} role={showSaved ? 'status' : undefined}>
            {showSaved ? <><CircleCheck size={14} aria-hidden="true" /><span>保存成功</span></> : loading ? '读取中…' : dirty ? '有未保存修改' : ''}
          </span>
          <button type="button" className="primary-button small" disabled={!dirty || saving || loading} onClick={() => void save()}>{saving ? '保存中…' : '保存'}</button>
        </footer>
      </motion.section>
    </div>
  </PageFrame>
}

export function SkillsPage() {
  const state = useAppStore(['bootstrap', 'selectedWorkspaceRoot'])
  const firstWorkspace = state.bootstrap?.workspaces[0]
  const root = state.selectedWorkspaceRoot ?? (firstWorkspace ? workspaceKey(firstWorkspace.root) : null)
  return <SkillsList key={root ?? 'none'} root={root} />
}

function SkillsList({ root }: { root: string | null }) {
  const [skills, setSkills] = useState<SkillMetadata[] | null>(root === null ? [] : null)
  const [query, setQuery] = useState('')
  const [selected, setSelected] = useState<SkillMetadata | null>(null)
  const [loading, setLoading] = useState(root !== null)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  const refresh = () => {
    if (loading || root === null) return
    setLoading(true)
    setReload(value => value + 1)
    appStore.invalidateContextPreview()
  }
  useEffect(() => {
    let active = true
    setError(null)
    setLoading(root !== null)
    if (root !== null) void appStore.transport.rpc('skills.list', { workspaceRoot: root }).then(catalog => {
      if (active) setSkills(catalog.skills)
    }).catch(error => { if (active) setError(error instanceof Error ? error.message : String(error)) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [root, reload])
  if (root !== null && selected !== null) {
    return <SkillDetail key={selected.path} skill={selected} workspaceRoot={root} onBack={() => setSelected(null)} />
  }
  const trimmed = query.trim().toLowerCase()
  const filtered = skills?.filter(skill => !trimmed || skill.name.toLowerCase().includes(trimmed) || skill.description.toLowerCase().includes(trimmed)) ?? null
  return <PageFrame title="Skills" actions={<>
    <span className="skills-count" role="status" aria-label={skills !== null ? `共 ${skills.length} 个技能` : '技能数量尚未读取'}>{skills !== null ? `${skills.length} 个` : '—'}</span>
    <button type="button" className="skills-refresh" aria-label="刷新 Skills 列表" title="刷新 Skills 列表" aria-busy={loading} aria-disabled={loading || root === null} onClick={refresh}>
      <RefreshCw size={15} aria-hidden="true" />
    </button>
  </>}>
    {skills !== null && skills.length > 0 && (
      <label className="skills-search">
        <Search size={14} aria-hidden="true" />
        <input type="search" aria-label="搜索 Skills" placeholder="搜索技能名称或说明…" value={query} onChange={event => setQuery(event.target.value)} />
      </label>
    )}
    {loading && skills === null && <p role="status">读取中…</p>}
    {error && <div className="form-error" role="alert">{error}</div>}
    {!loading && !error && skills?.length === 0 && <p className="detail-empty">没有可用的 Skills</p>}
    {!loading && !error && filtered !== null && skills !== null && skills.length > 0 && filtered.length === 0 && <p className="detail-empty">没有匹配的 Skills</p>}
    {root && filtered && filtered.length > 0 && (
      <div className="skill-list">{filtered.map(skill => <SkillCard key={`${root}:${skill.name}`} skill={skill} onOpen={() => setSelected(skill)} />)}</div>
    )}
  </PageFrame>
}

function SkillDetail({ skill, workspaceRoot, onBack }: { skill: SkillMetadata; workspaceRoot: string; onBack: () => void }) {
  const [document, setDocument] = useState<TextDocument | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [showSource, setShowSource] = useState(false)
  const [attempt, setAttempt] = useState(0)
  const reduced = useReducedMotion()
  useEffect(() => {
    let active = true
    setLoading(true)
    setError(null)
    void appStore.transport.rpc('file.read', { cwd: workspaceRoot, path: skill.path }).then(
      loaded => { if (active) setDocument(loaded) },
      reason => { if (active) setError(reason instanceof Error ? reason.message : String(reason)) },
    ).finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [workspaceRoot, skill.path, attempt])

  return (
    <motion.div
      className="manage-page skill-detail"
      initial={{ opacity: 0, x: reduced ? 0 : 12 }}
      animate={{ opacity: 1, x: 0 }}
      transition={reduced ? { duration: 0 } : smooth}
    >
      <div className="skill-detail-nav">
        <button type="button" className="quiet-button skill-detail-back" onClick={onBack}>
          <ArrowLeft size={14} strokeWidth={1.75} aria-hidden="true" />
          <span>返回 Skills</span>
        </button>
        {document && (
          <button type="button" className="quiet-button" onClick={() => setShowSource(value => !value)}>
            {showSource ? '预览' : '源码'}
          </button>
        )}
      </div>
      <header className="skill-detail-header">
        <h3>{skill.name}</h3>
        <span className="editor-path">{document?.path ?? skill.path}</span>
      </header>
      <div className="skill-detail-body">
        {loading && !document && <p className="detail-empty" role="status">读取中…</p>}
        {error && (
          <div className="form-error" role="alert">
            {error}{' '}
            <button type="button" className="quiet-button" onClick={() => setAttempt(value => value + 1)}>重试</button>
          </div>
        )}
        {document && (showSource
          ? <CodeView code={document.content} language="markdown" />
          : <MarkdownDocumentView path={document.path} content={document.content} />)}
      </div>
    </motion.div>
  )
}

function SkillCard({ skill, onOpen }: { skill: SkillMetadata; onOpen: () => void }) {
  return <button type="button" className="skill-card" onClick={onOpen}>
    <span className="skill-card-main">
      <strong>{skill.name}</strong>
      <span className="skill-description">{skill.description}</span>
    </span>
    <ChevronRight size={14} className="skill-row-chev" aria-hidden="true" />
  </button>
}

export function McpPage() {
  const { selectedWorkspaceRoot } = useAppStore(['selectedWorkspaceRoot'])
  return <PageFrame title="MCP"><McpSettings workspaceRoot={selectedWorkspaceRoot} /></PageFrame>
}
