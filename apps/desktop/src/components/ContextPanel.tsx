import { memo, useId, useState } from 'react'
import { useAppStore } from '../appStore'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'
import type { InstructionFile } from '../protocol'

export const ContextPanel = memo(function ContextPanel() {
  const state = useAppStore(['selectedSessionId', 'session', 'sessionLoad'], (a, b) =>
    a.selectedSessionId === b.selectedSessionId && a.session?.context === b.session?.context && a.sessionLoad === b.sessionLoad)
  if (state.selectedSessionId === null) return <p className="detail-empty">选择任务后查看上下文。</p>
  if (state.sessionLoad.status === 'loading') return <p className="detail-empty" role="status">正在读取任务…</p>
  if (state.sessionLoad.status === 'error') return <p className="detail-empty" role="alert">{state.sessionLoad.error?.message}</p>
  const context = state.session?.context
  if (!context) return <p className="detail-empty">本次打开后还未加载上下文。开始执行后，这里会显示实际加载的指令、Skills 和工具。</p>
  return <div className="context-panel" key={state.selectedSessionId}>
    <section aria-label="指令"><h3>指令<span>{context.instructions.length}</span></h3>
      {context.instructions.map(instruction => <Instruction key={instruction.path} instruction={instruction} />)}
    </section>
    <Names title="Skills" names={context.skills} />
    <Names title="工具" names={context.tools} />
  </div>
})
function Instruction({ instruction }: { instruction: InstructionFile }) {
  const [open, setOpen] = useState(false)
  const id = useId()
  return <div className="context-instruction">
    <button type="button" className="activity-toggle" aria-expanded={open} aria-controls={id} onClick={() => setOpen(value => !value)}>
      <ExpandChevron expanded={open} size={14} /><span className="instruction-label"><span>{instruction.global ? '全局指令' : '项目指令'}</span><small title={instruction.path}>{instruction.path}</small></span>
    </button>
    <div id={id}><Disclosure open={open}><pre className="instruction-body">{instruction.content}</pre></Disclosure></div>
  </div>
}

function Names({ title, names }: { title: string; names: string[] }) {
  const [open, setOpen] = useState(false)
  const id = useId()
  return <section aria-label={title}>
    <button type="button" className="activity-toggle context-group" aria-expanded={open} aria-controls={id} onClick={() => setOpen(value => !value)}>
      <ExpandChevron expanded={open} size={14} /><span>{title}</span><span className="context-count">{names.length}</span>
    </button>
    <div id={id}><Disclosure open={open}>
      {names.length ? <ul className="context-names">{names.map(name => <li key={name}>{name}</li>)}</ul> : <p className="detail-empty">暂无{title === 'Skills' ? ' Skills' : title}。</p>}
    </Disclosure></div>
  </section>
}
