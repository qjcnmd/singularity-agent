import { useEffect, useRef, useState, type ReactNode } from 'react'
import { motion, useReducedMotion } from 'motion/react'
import { appStore, useAppStore } from '../appStore'
import type { SkillMetadata, TextDocument } from '../protocol'
import { workspaceName } from '../copy'
import { workspaceKey } from '../workspace'
import { smooth } from '../motion'
import { SelectionSlider } from './SelectionSlider'
import { McpSettings } from './McpSettings'
import { ChevronRight } from 'lucide-react'

function PageFrame({ title, children }: { title: string; children: ReactNode }) {
  const reduced = useReducedMotion()
  return <div className="manage-scroll">
    <motion.div className="manage-page" initial={{ opacity: 0, y: 8 }} animate={{ opacity: 1, y: 0 }} transition={reduced ? { duration: 0 } : smooth}>
      <header><h2>{title}</h2></header>{children}
    </motion.div>
  </div>
}

export function InstructionsPage() {
  const state = useAppStore(['bootstrap'])
  const [target, setTarget] = useState<string | null>(null)
  const [documents, setDocuments] = useState<Record<string, TextDocument>>({})
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  const nav = useRef<HTMLElement>(null)
  const reduced = useReducedMotion()
  const key = target ?? 'global'
  const saved = documents[key]
  const text = drafts[key] ?? saved?.content ?? ''
  const dirty = saved !== undefined && text !== saved.content
  useEffect(() => {
    let active = true
    setLoading(true)
    setError(null)
    void appStore.transport.rpc('instructions.read', { workspaceRoot: target }).then(document => {
      if (active) setDocuments(current => ({ ...current, [key]: document }))
    }).catch(error => { if (active) setError(error instanceof Error ? error.message : String(error)) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [target, key, reload])
  async function save() {
    setSaving(true)
    setError(null)
    try {
      const document = await appStore.transport.rpc('instructions.save', { workspaceRoot: target, content: text })
      setDocuments(current => ({ ...current, [key]: document }))
      setDrafts(current => { const next = { ...current }; delete next[key]; return next })
    } catch (error) { setError(error instanceof Error ? error.message : String(error)) }
    finally { setSaving(false) }
  }
  return <PageFrame title="自定义指令">
    <div className="instructions-layout">
      <nav ref={nav} className="instruction-targets" aria-label="指令范围">
        <SelectionSlider container={nav} activeKey={key} />
        <button type="button" disabled={saving} data-selected={target === null} aria-current={target === null ? 'true' : undefined} onClick={() => setTarget(null)}><span className="target-label"><strong>全局指令</strong></span></button>
        {state.bootstrap?.workspaces.map(workspace => {
          const root = workspaceKey(workspace.root)
          return <button key={root} type="button" disabled={saving} data-selected={target === root} aria-current={target === root ? 'true' : undefined} onClick={() => setTarget(root)}>
            <span className="target-label"><strong>{workspaceName(workspace)}</strong></span>
          </button>
        })}
      </nav>
      <motion.section key={key} className="instruction-editor" initial={{ opacity: 0 }} animate={{ opacity: 1 }} transition={{ duration: reduced ? 0 : .18 }} aria-busy={loading || saving}>
        {saved && <div className="editor-path">{saved.path}</div>}
        {loading && <span role="status">读取中…</span>}
        {error && <div className="form-error" role="alert">{error}{!saved && <button type="button" className="quiet-button" onClick={() => setReload(value => value + 1)}>重新读取</button>}</div>}
        <textarea aria-label="自定义指令内容" className="input" value={text} spellCheck={false} disabled={loading || saving || !saved}
          onChange={event => setDrafts(current => ({ ...current, [key]: event.target.value }))} />
        <footer>
          <button type="button" className="secondary-button" disabled={!dirty || saving || loading} onClick={() => setDrafts(current => { const next = { ...current }; delete next[key]; return next })}>撤销修改</button>
          <button type="button" className="primary-button" disabled={!dirty || saving || loading} onClick={() => void save()}>{saving ? '保存中…' : '保存'}</button>
        </footer>
      </motion.section>
    </div>
  </PageFrame>
}

export function SkillsPage() {
  const state = useAppStore(['bootstrap', 'selectedWorkspaceRoot'])
  const root = state.selectedWorkspaceRoot ?? state.bootstrap?.workspaces[0]?.root ?? null
  const [skills, setSkills] = useState<SkillMetadata[]>([])
  const [loading, setLoading] = useState(root !== null)
  const [error, setError] = useState<string | null>(null)
  const [reload, setReload] = useState(0)
  useEffect(() => {
    let active = true
    setSkills([])
    setError(null)
    setLoading(root !== null)
    if (root !== null) void appStore.transport.rpc('skills.list', { workspaceRoot: root }).then(catalog => {
      if (active) setSkills(catalog.skills)
    }).catch(error => { if (active) setError(error instanceof Error ? error.message : String(error)) })
      .finally(() => { if (active) setLoading(false) })
    return () => { active = false }
  }, [root, reload])
  return <PageFrame title="Skills">
    {loading && <p role="status">读取中…</p>}
    {error && <div className="form-error" role="alert">{error}<button type="button" className="quiet-button" onClick={() => setReload(value => value + 1)}>重新读取</button></div>}
    {!loading && !error && skills.length === 0 && <p className="detail-empty">没有可用的 Skills</p>}
    <div className="card-list">{root && skills.map(skill => <SkillCard key={`${root}:${skill.name}`} skill={skill} workspaceRoot={root} />)}</div>
  </PageFrame>
}

/** 点开在右侧文件栏查看 SKILL.md，篇幅再长也不用滚回顶部收起。 */
function SkillCard({ skill, workspaceRoot }: { skill: SkillMetadata; workspaceRoot: string }) {
  return <div className="row-card skill-card">
    <div className="row-card-head"><button type="button" className="row-card-toggle" onClick={() => appStore.openFile(skill.path, workspaceRoot, skill.name)}>
      <span className="provider-avatar" aria-hidden="true">/</span><span className="row-card-title"><strong>{skill.name}</strong><small>{skill.description}</small></span><ChevronRight size={16} aria-hidden="true" />
    </button></div>
  </div>
}

export function McpPage() {
  const { selectedWorkspaceRoot } = useAppStore(['selectedWorkspaceRoot'])
  const [busy, setBusy] = useState(false)
  return <PageFrame title="MCP"><McpSettings workspaceRoot={selectedWorkspaceRoot} active busy={busy} onBusyChange={setBusy} /></PageFrame>
}
