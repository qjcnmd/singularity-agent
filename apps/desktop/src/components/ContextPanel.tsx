import { memo, useEffect, useState } from 'react'
import { appStore, useAppStore } from '../appStore'
import { CapsuleIcon, CapsuleTokens } from './ContextCapsules'

export const ContextPanel = memo(function ContextPanel() {
  const state = useAppStore(['selectedSessionId', 'session'], (a, b) =>
    a.selectedSessionId === b.selectedSessionId && a.session?.context === b.session?.context)
  const context = state.session?.context
  const sessionId = state.selectedSessionId
  // 本次启动还没跑过回合时没有上下文：请后端按回合开始时的规则加载一次，结果随 context/loaded 事件送达。
  const [failure, setFailure] = useState<{ sessionId: string; message: string } | null>(null)
  const [attempt, setAttempt] = useState(0)
  const missing = sessionId !== null && state.session !== null && !context
  useEffect(() => {
    if (!missing || sessionId === null) return
    setFailure(null)
    void appStore.transport.rpc('session.context', { sessionId }).catch(error =>
      setFailure({ sessionId, message: error instanceof Error ? error.message : String(error) }))
  }, [missing, sessionId, attempt])
  if (!context) {
    if (failure?.sessionId === sessionId) return <p className="detail-empty form-error" role="alert">{failure.message}
      <button type="button" className="quiet-button" onClick={() => setAttempt(value => value + 1)}>重试</button></p>
    return <p className="detail-empty" role="status">读取中…</p>
  }
  // 只列本任务开着的模块，和实际发送给模型的内容一致；胶囊只展示，不能在这里改。
  return <div className="context-panel" key={state.selectedSessionId}>
    <div className="capsules">
      {context.modules.filter(module => module.enabled).map(module => <span key={module.id} className="capsule capsule-static is-on" title={module.detail}>
        <CapsuleIcon /><span className="capsule-name">{module.label}</span><CapsuleTokens tokens={module.tokens} />
      </span>)}
    </div>
  </div>
})
