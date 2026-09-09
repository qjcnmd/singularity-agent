import { CopyButton } from './CopyButton'
import { ExpandChevron } from './ExpandChevron'
import { memo, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { ModelRequestSnapshot } from '../protocol'
import { buildTrajectory, systemText, type TrajectoryEntry } from '../trajectory'
import { hasTextSelection } from '../interactions'
import { MarkdownBody } from './TimelineItem'
import { workbenchStore, useWorkbenchStore } from '../store'
import { diffLines } from 'diff'
import { AnimatePresence, motion, useReducedMotion } from 'motion/react'

const statuses = { stable: '已记录', running: '进行中', ok: '已完成', error: '失败', cancelled: '已停止' }
const seconds = (value: number | null | undefined) => value == null ? '未知' : `${(value / 1000).toFixed(2)} 秒`
const pretty = (value: unknown) => JSON.stringify(value, null, 2) ?? ''

type Selection = { key: string; request: boolean }
type Row = { key: string; entry: TrajectoryEntry; turn: string; turnTitle: string; parent: string | null }

export const Trajectory = memo(TrajectoryView)

function TrajectoryView({ visible }: { visible: boolean }) {
  const { session } = useWorkbenchStore(['session'])
  const turns = useMemo(() => buildTrajectory(session), [session])
  const rows = useMemo(() => {
    const result: Row[] = []
    for (const turn of turns) {
      let parent: string | null = null
      for (const item of turn.entries) {
        const key = `${turn.id}:${item.id}`
        if (item.kind === 'assistant') parent = key
        result.push({ key, entry: item, turn: turn.id, turnTitle: turn.title, parent: item.kind === 'tool' ? parent : null })
      }
    }
    return result
  }, [turns])
  const [foldedTurns, setFoldedTurns] = useState<Set<string>>(new Set())
  const [foldedCalls, setFoldedCalls] = useState<Set<string>>(new Set())
  const [selected, setSelected] = useState<Selection | null>(null)
  const [tab, setTab] = useState('summary')
  const container = useRef<HTMLDivElement>(null)
  const backButton = useRef<HTMLButtonElement>(null)
  const follow = useRef(true)
  const scrollOffset = useRef(0)
  const returnSelection = useRef<Selection | null>(null)
  const reducedMotion = useReducedMotion()
  const transition = { duration: reducedMotion ? 0 : 0.16, ease: 'easeOut' as const }
  const selectedRow = rows.find(row => row.key === selected?.key)
  const rowIndex = useMemo(() => {
    const firstByTurn = new Map<string, string>()
    const children = new Map<string, number>()
    for (const row of rows) {
      if (!firstByTurn.has(row.turn)) firstByTurn.set(row.turn, row.key)
      if (row.parent) children.set(row.parent, (children.get(row.parent) ?? 0) + 1)
    }
    return { firstByTurn, children }
  }, [rows])
  const inspect = (row: Row, request = false) => {
    setSelected({ key: row.key, request })
    setTab(request ? 'summary' : defaultTab(row.entry))
  }
  const back = () => {
    returnSelection.current = selected
    setSelected(null)
  }
  useLayoutEffect(() => {
    if (visible && !selected && follow.current && container.current) container.current.scrollTop = container.current.scrollHeight
  }, [rows, visible, selected])
  useEffect(() => { if (selected) backButton.current?.focus() }, [selected])
  const toggle = (value: string, setter: (update: (current: Set<string>) => Set<string>) => void) => setter(previous => { const next = new Set(previous); if (next.has(value)) next.delete(value); else next.add(value); return next })
  return <section className="trajectory-view" aria-label="轨迹记录与详情" onKeyDown={event => {
    if (event.key === 'Escape' && selected) { event.stopPropagation(); back() }
  }}>
    <AnimatePresence initial={false} mode="wait">
    {!selectedRow ? <motion.div key="ledger" className="trajectory-ledger" initial={{ opacity: 0, x: reducedMotion ? 0 : -8 }} animate={{ opacity: 1, x: 0 }} exit={{ opacity: 0, x: reducedMotion ? 0 : -8 }} transition={transition} ref={node => {
      container.current = node
      if (node) node.scrollTop = follow.current ? node.scrollHeight : scrollOffset.current
    }} onAnimationComplete={definition => {
      if (typeof definition !== 'object' || !('opacity' in definition) || definition.opacity !== 1) return
      const previous = returnSelection.current
      if (previous) container.current?.querySelector<HTMLElement>(`[data-trajectory-id="${CSS.escape(previous.key)}"] ${previous.request ? '.trajectory-request-number' : '.trajectory-preview'}`)?.focus({ preventScroll: true })
      returnSelection.current = null
    }} onScroll={event => { const node = event.currentTarget; scrollOffset.current = node.scrollTop; follow.current = node.scrollHeight - node.clientHeight - node.scrollTop < 8 }}>
      {session?.history.nextCursor && <button type="button" className="load-older" onClick={() => void workbenchStore.readOlder()}>加载更早记录</button>}
      <table aria-label="轨迹记录"><tbody>{rows.map(row => {
        const first = rowIndex.firstByTurn.get(row.turn) === row.key
        if ((!first && foldedTurns.has(row.turn)) || (row.parent && foldedCalls.has(row.parent))) return null
        const item = row.entry
        const childCount = rowIndex.children.get(row.key) ?? 0
        return <motion.tr layout="position" transition={transition} key={row.key} data-trajectory-id={row.key} className={item.failed ? 'is-failed' : undefined}>
          <td className="trajectory-role">
            {first && <button type="button" className="trajectory-turn-marker" aria-label={`${foldedTurns.has(row.turn) ? '展开' : '折叠'}${row.turnTitle}`} aria-expanded={!foldedTurns.has(row.turn)} title={row.turnTitle} onClick={() => toggle(row.turn, setFoldedTurns)}><ExpandChevron expanded={!foldedTurns.has(row.turn)} size={12} /></button>}
            {item.request && <button type="button" className="trajectory-request-number" aria-label={`${item.title} 详情`} onClick={() => inspect(row, true)} title={item.title}>{item.request.attempt}</button>}
            <span className={`trajectory-kind kind-${item.kind}`}>{item.kind}</span>
          </td>
          <td className="trajectory-preview-cell"><button type="button" className="trajectory-preview" onClick={() => { if (!hasTextSelection()) inspect(row) }}>
            {item.kind === 'tool' && <><strong className="execution-title">{item.title}</strong><code>{pretty(item.input)}</code>{item.text && <span className="trajectory-arrow">→</span>}</>}
            {!(item.kind === 'tool' && item.status === 'running' && !item.text) && <span>{item.kind === 'system' ? item.title : item.text || (item.thinking ? '（含思考内容）' : item.status === 'running' ? '正在生成…' : childCount ? '（仅工具调用）' : '—')}</span>}
          </button>{childCount > 0 && <button type="button" className="trajectory-call-toggle" aria-expanded={!foldedCalls.has(row.key)} aria-label={`${foldedCalls.has(row.key) ? '展开' : '折叠'} ${childCount} 次工具调用`} onClick={() => toggle(row.key, setFoldedCalls)}><ExpandChevron expanded={!foldedCalls.has(row.key)} size={12} />{childCount}</button>}</td>
        </motion.tr>
      })}</tbody></table>
      {!rows.length && <p className="candidate-message">尚无轨迹记录</p>}
    </motion.div>
    : <motion.div key="inspector" className="trajectory-inspector" aria-label="轨迹详情" initial={{ opacity: 0, x: reducedMotion ? 0 : 8 }} animate={{ opacity: 1, x: 0 }} exit={{ opacity: 0, x: reducedMotion ? 0 : 8 }} transition={transition} onAnimationComplete={definition => { if (typeof definition === 'object' && 'opacity' in definition && definition.opacity === 1) backButton.current?.focus() }}>
      <header><button ref={backButton} type="button" className="quiet-button trajectory-back" onClick={back}>← 返回轨迹</button><strong>{selected?.request ? selectedRow.entry.title : selectedRow.entry.kind}</strong><small title={selectedRow.turnTitle}>{selectedRow.turnTitle}</small></header>
      <Inspector key={`${selectedRow.key}:${selected?.request}`} row={selectedRow} request={selected?.request ?? false} tab={tab} setTab={setTab} onRequest={() => inspect(selectedRow, true)} />
    </motion.div>}
    </AnimatePresence>
  </section>
}


function defaultTab(item: TrajectoryEntry) { return item.kind === 'system' ? 'system' : 'summary' }
function Inspector({ row, request, tab, setTab, onRequest }: { row: Row; request: boolean; tab: string; setTab: (tab: string) => void; onRequest: () => void }) {
  const reducedMotion = useReducedMotion()
  const item = row.entry
  const snapshot = item.request?.request ?? item.prompt
  const tabs = request ? [['summary', '概览'], ['context', '上下文'], ['tools', '工具'], ['options', '选项'], ['usage', '用量'], ['timing', '时序'], ['raw', '原始数据']]
    : item.kind === 'system' ? [...(item.previousPrompt ? [['diff', '变更']] : []), ['system', '系统提示词'], ['tools', '工具']]
      : item.kind === 'tool' ? [['summary', '概览'], ['input', '输入'], ['output', '输出'], ['schema', '定义'], ['timing', '时序']]
        : [['summary', '概览'], ['rendered', '正文'], ['raw', '原始数据'], ...(item.thinking ? [['thinking', '思考']] : [])]
  const active = tabs.some(([id]) => id === tab) ? tab : tabs[0][0]
  const stats = item.request
  const content = active === 'input' ? pretty(item.input) : active === 'output' ? item.text : active === 'schema' ? pretty(item.schema) : active === 'raw' ? pretty(request ? stats : item) : active === 'thinking' ? item.thinking : ''
  return <>
    <div className="trajectory-detail-tabs" role="tablist" aria-label="轨迹详情栏目" onKeyDown={event => { if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return; event.preventDefault(); const index = tabs.findIndex(([id]) => id === active); const next = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : (index + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length; setTab(tabs[next][0]); (event.currentTarget.children[next] as HTMLButtonElement)?.focus() }}>{tabs.map(([id, label]) => <button key={id} type="button" role="tab" tabIndex={active === id ? 0 : -1} aria-selected={active === id} onClick={() => setTab(id)}>{label}</button>)}</div>
    <AnimatePresence initial={false} mode="wait"><motion.div key={active} className="trajectory-detail-body" role="tabpanel" aria-label={tabs.find(([id]) => id === active)?.[1]} initial={{ opacity: 0 }} animate={{ opacity: 1 }} exit={{ opacity: 0 }} transition={{ duration: reducedMotion ? 0 : 0.12 }}>
      {request && stats?.requestError && <p className="candidate-message" role="alert">请求详情不可用：{stats.requestError}</p>}
      {active === 'summary' && <><dl className="trajectory-facts"><div><dt>状态</dt><dd>{statuses[item.status]}</dd></div>{stats && <><div><dt>提供方</dt><dd>{stats.provider}</dd></div><div><dt>模型</dt><dd>{stats.model}</dd></div></>}<div><dt>耗时</dt><dd>{seconds(item.duration)}</dd></div>{item.failed && <div><dt>错误</dt><dd>{stats?.error ?? item.text}</dd></div>}</dl>
        {!request && stats && <button type="button" className="quiet-button" onClick={onRequest}>查看 {item.title} →</button>}
        {request ? <><h4>用量</h4><Usage item={item} /><h4>请求选项</h4><JsonValue value={snapshot?.model_preferences ?? null} /></> : item.kind === 'tool' ? <><h4>输入</h4><JsonValue value={item.input} /><h4>输出</h4><Payload text={item.text} /></> : item.kind === 'user' ? <div className="user-text">{item.text}</div> : <MarkdownBody text={item.text || item.thinking || '（仅工具调用）'} />}
      </>}
      {active === 'rendered' && <MarkdownBody text={item.text} />}
      {active === 'system' && <MarkdownBody text={snapshot ? systemText(snapshot) : item.text} />}
      {active === 'diff' && item.previousPrompt && snapshot && <PromptChanges before={item.previousPrompt} after={snapshot} />}
      {active === 'context' && (snapshot ? snapshot.messages.map((message, index) => <details className="trajectory-context-message" key={index} open={index === snapshot.messages.length - 1}><summary>{index + 1} · {message.role}<span>{message.content.length.toLocaleString()} 字符</span></summary><MarkdownBody text={message.content} />{message.tool_calls?.length ? <JsonValue value={message.tool_calls} /> : null}</details>) : <p>此请求未记录完整上下文。</p>)}
      {active === 'tools' && (snapshot ? <ToolCatalog snapshot={snapshot} /> : <p>此请求未记录工具定义。</p>)}
      {active === 'options' && <JsonValue value={snapshot?.model_preferences ?? null} />}
      {active === 'usage' && <Usage item={item} />}
      {active === 'timing' && <dl className="trajectory-facts"><div><dt>总耗时</dt><dd>{seconds(item.duration)}</dd></div>{item.startedAt && <div><dt>开始</dt><dd>{item.startedAt}</dd></div>}</dl>}
      {['input', 'output', 'schema', 'raw', 'thinking'].includes(active) && <Payload text={content || '未记录'} />}
    </motion.div></AnimatePresence>
  </>
}
function Usage({ item }: { item: TrajectoryEntry }) {
  return <dl className="trajectory-facts">{[['输入 Token', item.request?.inputTokens], ['输出 Token', item.request?.outputTokens], ['缓存输入 Token', item.request?.cachedInputTokens]].map(([label, value]) => <div key={label}><dt>{label}</dt><dd>{value == null ? '未知' : Number(value).toLocaleString()}</dd></div>)}</dl>
}
function ToolCatalog({ snapshot }: { snapshot: ModelRequestSnapshot }) {
  return <div>{snapshot.tools.map(tool => <details className="trajectory-context-message" key={tool.name}><summary>{tool.name}</summary><MarkdownBody text={tool.description} /><JsonValue value={tool.parameters_schema} /></details>)}</div>
}
function PromptChanges({ before, after }: { before: ModelRequestSnapshot; after: ModelRequestSnapshot }) {
  const sections = useMemo(() => [
    { title: '系统提示词', before: systemText(before), after: systemText(after) },
    { title: '工具定义', before: pretty(before.tools), after: pretty(after.tools) },
  ].filter(section => section.before !== section.after).map(section => ({ title: section.title, changes: diffLines(section.before, section.after) })), [before, after])
  return <>{sections.map(section => <section key={section.title}><h4>{section.title}</h4><pre className="trajectory-prompt-diff">{section.changes.map((change, index) => <span key={index} className={change.added ? 'diff-added' : change.removed ? 'diff-removed' : undefined}>{change.value.split(/(?<=\n)/).map(line => `${change.added ? '+' : change.removed ? '−' : ' '} ${line}`).join('')}</span>)}</pre></section>)}</>
}
function JsonValue({ value }: { value: unknown }) { return <Payload text={pretty(value)} /> }
function Payload({ text }: { text: string }) {
  return <div className="trajectory-payload"><CopyButton text={text} /><pre>{text}</pre></div>
}
