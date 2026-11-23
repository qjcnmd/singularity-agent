import { memo, useEffect, useState } from 'react'
import { appStore, useAppStore } from '../appStore'
import { CapsuleIcon, CapsuleTokens } from './ContextCapsules'

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
  return <div className="context-panel" key={state.selectedSessionId}>
    <div className="capsules">
      {context.modules.filter(module => module.enabled).map(module => <span key={module.id} className="capsule capsule-static is-on" title={module.detail}>
        <CapsuleIcon /><span className="capsule-name">{module.label}</span><CapsuleTokens tokens={module.tokens} />
      </span>)}
    </div>
  </div>
})
