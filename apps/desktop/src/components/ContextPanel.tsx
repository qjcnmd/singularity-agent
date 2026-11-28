import { memo, useEffect, useState } from 'react'
import { appStore, useAppStore } from '../appStore'
import { formatTokenCount } from '../copy'
import type { ContextGroup, ContextModule } from '../protocol'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

const groupOrder: ContextGroup[] = ['instructions', 'tools', 'skills', 'mcp']
const groupLabels: Record<ContextGroup, string> = {
  instructions: 'Instructions',
  tools: 'Tools',
  skills: 'Skills',
  mcp: 'MCP',
}

function formatApproxTokens(tokens: number): string {
  return tokens > 0 ? `~${formatTokenCount(tokens).toLowerCase()}` : ''
}

export const ContextPanel = memo(function ContextPanel() {
  const state = useAppStore(['selectedSessionId', 'session'], (a, b) =>
    a.selectedSessionId === b.selectedSessionId
    && (a.session === null) === (b.session === null)
    && a.session?.context === b.session?.context
    && a.session?.runtime.phase === b.session?.runtime.phase
    && a.session?.runtime.selection.provider === b.session?.runtime.selection.provider
    && a.session?.runtime.selection.model === b.session?.runtime.selection.model
    && a.session?.runtime.selection.reasoning === b.session?.runtime.selection.reasoning)
  const context = state.session?.context
  const sessionId = state.selectedSessionId
  const phase = state.session?.runtime.phase
  const selection = state.session?.runtime.selection
  const [failure, setFailure] = useState<{ sessionId: string; message: string } | null>(null)
  const [attempt, setAttempt] = useState(0)
  const missing = sessionId !== null && state.session !== null && !context
  useEffect(() => {
    if (!missing || sessionId === null || phase !== 'idle') {
      setFailure(null)
      return
    }
    let cancelled = false
    setFailure(null)
    void appStore.transport.rpc('session.context', { sessionId }).catch(error => {
      if (!cancelled) setFailure({ sessionId, message: error instanceof Error ? error.message : String(error) })
    })
    return () => { cancelled = true }
  }, [missing, sessionId, phase, selection?.provider, selection?.model, selection?.reasoning, attempt])
  if (!context) {
    if (failure?.sessionId === sessionId) return <p className="detail-empty form-error" role="alert">{failure.message}
      <button type="button" className="quiet-button" onClick={() => setAttempt(value => value + 1)}>重试</button></p>
    return <p className="detail-empty" role="status">读取中…</p>
  }
  const enabled = context.filter(module => module.enabled)
  return <div className="context-panel" key={state.selectedSessionId}>
    {groupOrder.map(group => {
      const members = enabled.filter(module => module.group === group)
      return members.length > 0 ? <ContextGroupRow key={group} group={group} members={members} /> : null
    })}
  </div>
})

function ContextGroupRow({ group, members }: { group: ContextGroup; members: ContextModule[] }) {
  const [open, setOpen] = useState(false)
  const title = groupLabels[group]
  const totalTokens = members.reduce((sum, module) => sum + module.tokens, 0)
  if (members.length === 1) {
    const solo = members[0]
    const sublabel = solo.label.toLowerCase() === title.toLowerCase() ? null : solo.label
    return <div className="context-group">
      <div className="context-group-row is-static" title={solo.detail}>
        <span className="context-group-name">
          <span>{title}</span>
          {sublabel && <span className="context-group-sub">{sublabel}</span>}
        </span>
        <span className="context-group-tokens">{formatApproxTokens(totalTokens)}</span>
      </div>
    </div>
  }
  return <div className="context-group">
    <button type="button" className="context-group-row" aria-expanded={open} onClick={() => setOpen(value => !value)}>
      <span className="context-group-name">
        <span>{title}</span>
        <span className="context-group-count">{members.length}</span>
        <ExpandChevron expanded={open} size={12} />
      </span>
      <span className="context-group-tokens">{formatApproxTokens(totalTokens)}</span>
    </button>
    <Disclosure open={open}>
      <ul className="context-group-items">
        {members.map(module => <li key={module.id} className="context-item-row" title={module.detail}>
          <span className="context-item-name">{module.label}</span>
          <span className="context-item-tokens">{formatApproxTokens(module.tokens)}</span>
        </li>)}
      </ul>
    </Disclosure>
  </div>
}
