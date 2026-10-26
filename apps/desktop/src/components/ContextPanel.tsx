import { memo, useEffect, useId, useState, type ReactNode } from 'react'
import { appStore, useAppStore } from '../appStore'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import type { InstructionFile } from '../protocol'

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
  return <div className="context-panel" key={state.selectedSessionId}>
    <Group title="指令" count={context.instructions.length}>
      {context.instructions.map(instruction => <Instruction key={instruction.path} instruction={instruction} />)}
    </Group>
    <Names title="系统工具" names={context.tools} />
    {context.mcpTools.length > 0 && <Names title="MCP 工具" names={context.mcpTools} />}
  </div>
})
function Instruction({ instruction }: { instruction: InstructionFile }) {
  const [open, setOpen] = useState(false)
  const id = useId()
  return <div className="context-instruction">
    <button type="button" className="expandable-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen(value => !value)}>
      <ExpandChevron expanded={open} size={14} /><span className="instruction-label"><span>{instruction.global ? '全局指令' : '项目指令'}</span><small>{instruction.path}</small></span>
    </button>
    <div id={id}><Disclosure open={open}><pre className="instruction-body">{instruction.content}</pre></Disclosure></div>
  </div>
}

function Names({ title, names }: { title: string; names: string[] }) {
  return <Group title={title} count={names.length}><ul className="name-list">{names.map(name => <li key={name} title={name}>{name}</li>)}</ul></Group>
}

function Group({ title, count, children }: { title: string; count: number; children: ReactNode }) {
  const [open, setOpen] = useState(false)
  const id = useId()
  return <section aria-label={title}>
    <button type="button" className="expandable-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen(value => !value)}>
      <ExpandChevron expanded={open} size={13} /><strong>{title}</strong><span className="count">{count}</span>
    </button><div id={id}><Disclosure open={open}><div className="expandable-body">{children}</div></Disclosure></div>
  </section>
}
