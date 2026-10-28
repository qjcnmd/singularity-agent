import { ExpandChevron } from './ExpandChevron'
import { AttachedImages, FileChips } from './Images'
import { QuestionPanel } from './QuestionPanel'
import { Disclosure } from './Disclosure'
import { ToolGroupRow } from './ToolGroupRow'
import { toolDisplay } from '../toolDisplay'
import { memo, useEffect, useState, type ReactNode } from 'react'
import { CommandOutput } from './CommandOutput'
import { CircleAlert, CircleCheck, CircleX, LoaderCircle, FileText, SquareTerminal, Image, MessageCircleQuestion } from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'
import { enter, pop } from '../motion'
import { useAppStore } from '../appStore'
import { useBodyCollapse, useSelectionGuard } from '../interactions'
import { MarkdownBody } from '../markdown'
import { compactionText, factStatusText, formatTokenCount } from '../copy'
import { readOutputLines } from '../readOutput'
import { formatClock, compactionResultText, displayPath, firstLine, formatTurnDuration, pathFromArgs, toolSummary, toolArgument, type TimelineContent } from '../timeline'
import type { HistoryItem, SessionModelUsage, ToolStatus } from '../protocol'

const previewLineCount = 8

interface Props {
  item: TimelineContent
  itemKey: string
  cwd: string
  userHome?: string | null
  sessionId?: string
  onDisclosureChange: (element: HTMLElement) => void
}

// 历史项复用稳定引用跳过重绘；流式更新只重新渲染当前活动项。
export const TimelineItem = memo(function TimelineItem({ item, itemKey, cwd, userHome, sessionId, onDisclosureChange }: Props) {
  const userLines = item.type === 'message' && item.role === 'user' ? item.text.trimEnd().split('\n') : []
  const hiddenLines = Math.max(0, userLines.length - previewLineCount)
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(item.type !== 'tool')
  const selectionGuard = useSelectionGuard(onDisclosureChange)
  const reduced = useReducedMotion()

  if (item.type === 'tool_group') return <ToolGroupRow group={item} itemKey={itemKey} sessionId={sessionId} onDisclosureChange={onDisclosureChange}>
    {item.rows.map(row => <ToolCard key={row.key} item={row.item} itemKey={`${row.key}:group-child`} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />)}
  </ToolGroupRow>
  if (item.type === 'duration') return <TurnDuration itemKey={itemKey} {...item} />
  if (item.type === 'question') return <motion.article {...enter} transition={reduced ? { duration: 0 } : pop} style={{ originX: 0, originY: 1 }}
    className="timeline-item message-item timeline-assistant timeline-question" data-item-id={itemKey}>
    <QuestionPanel key={item.request.itemId} request={item.request} />
  </motion.article>
  if (item.type === 'typing') return <motion.article {...enter} transition={reduced ? { duration: 0 } : pop} style={{ originX: 0, originY: 1 }}
    className="timeline-item message-item timeline-assistant" data-item-id={itemKey} aria-label="正在输入">
    <div className="message-bubble typing-bubble"><i /><i /><i /></div>
  </motion.article>

  if (item.type === 'terminal') {
    return <div className={`timeline-item message-item timeline-assistant${item.status === 'interrupted' ? ' terminal-interruption' : ''}`} data-item-id={itemKey}>
      {item.status === 'failed'
        ? <div className="message-bubble error-bubble" role="alert">{item.text}</div>
        : <article className="activity-step"><div className="activity-toggle"><CircleX size={13} className="bad" /><span className="step-title">{item.text}</span></div></article>}
      {item.at && <time className="turn-stats" dateTime={item.at} title="结束时间">{formatClock(item.at, true)}</time>}
    </div>
  }

  if (item.type === 'message') {
    const body = canCollapse && !expanded ? userLines.slice(0, previewLineCount).join('\n') : item.text
    const attachments = <>
      {sessionId && item.images && <AttachedImages sessionId={sessionId} images={item.images} />}
      {item.files && <FileChips files={item.files} />}
    </>
    return (
      <motion.article initial={{ opacity: 0, y: item.role === 'user' ? 36 : 10, scale: item.role === 'user' ? .84 : .9 }} animate={{ opacity: 1, y: 0, scale: 1 }} transition={reduced ? { duration: 0 } : { ...pop, opacity: { duration: .16 } }} style={{ originX: item.role === 'user' ? 1 : 0, originY: 1 }}
        className={`timeline-item message-item timeline-${item.role} status-${item.status}`}
        data-item-id={itemKey}
        aria-label={`${item.role === 'user' ? '你' : 'Singularity'}，${factStatusText[item.status]}`}
      >
        {item.role === 'user' && item.at && <time className="msg-time" dateTime={item.at}>{formatClock(item.at, true)}</time>}
        <div className={`message-bubble${item.status === 'failed' ? ' error-bubble' : ''}`}>
        {item.role === 'user' && attachments}
        <div className="timeline-body message-body">{item.role === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} streaming={item.status === 'running'} />}</div>
        {item.role === 'assistant' && attachments}
        {canCollapse && (
          <button type="button" className="expand-button" aria-expanded={expanded} {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${hiddenLines} 行`}
          </button>
        )}
        </div>
        {item.role === 'assistant' && <MessageStatsLine stats={item.stats} at={item.at} />}
      </motion.article>
    )
  }

  if (item.type === 'thinking') return <ReasoningRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />
  if (item.type === 'compaction' || item.type === 'compaction_result') return <CompactionRow item={item} itemKey={itemKey} onDisclosureChange={onDisclosureChange} />

  return <ToolCard item={item} itemKey={itemKey} cwd={cwd} userHome={userHome} sessionId={sessionId} onDisclosureChange={onDisclosureChange} />
})

function ToolCard({ item, itemKey, cwd, userHome, sessionId, onDisclosureChange }: Omit<Props, 'item'> & { item: Extract<HistoryItem, { type: 'tool' }> }) {
  const reduced = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const selectionGuard = useSelectionGuard(onDisclosureChange)
  const { liveSessions } = useAppStore(['liveSessions'], (previous, next) =>
    previous.liveSessions[sessionId ?? '']?.backgroundTasks === next.liveSessions[sessionId ?? '']?.backgroundTasks)
  const { status, shown } = toolDisplay(item, liveSessions[sessionId ?? '']?.backgroundTasks)
  const filePath = pathFromArgs(item.name, item.args)
  const title = item.name === 'ask_user_question' ? '询问用户' : item.name === 'skill' ? '读取技能' : item.name
  const summary = filePath !== null ? displayPath(filePath, cwd, userHome) : toolSummary(item.name, item.args) || firstLine(item.result?.output ?? '')

  return (
    <motion.article {...enter} transition={reduced ? { duration: 0 } : pop} className={`timeline-item activity-step timeline-tool status-${status}`} data-item-id={itemKey} aria-label={`${title}，${factStatusText[status]}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <StepLabel title={title} icon={<StepIcon name={item.name} status={status} />} />
        <span className="step-summary">{summary}</span>
        {status === 'interrupted' && <span className="item-status">{factStatusText.interrupted}</span>}
        {status === 'pending' && <span className="item-status">{factStatusText.pending}</span>}
        <ExpandChevron expanded={expanded} className="step-chevron" />
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        {shown.result?.durationMs !== undefined && <div className="tool-duration">耗时 {formatTurnDuration(shown.result.durationMs)}</div>}
        <div className="timeline-body activity-output"><ToolOutput item={shown} filePath={filePath} />{sessionId && item.result?.images && <AttachedImages sessionId={sessionId} images={item.result.images} />}</div>
      </div></Disclosure>
    </motion.article>
  )
}

function TurnDuration({ itemKey, startedAt, firstTokenAt, finishedAt }: { itemKey: string; startedAt: string; firstTokenAt?: string; finishedAt?: string }) {
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    if (finishedAt) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [finishedAt])
  const elapsed = (finishedAt ? Date.parse(finishedAt) : now) - Date.parse(startedAt)
  return <div className="turn-duration" data-item-id={itemKey} role={finishedAt ? undefined : 'timer'} aria-label="本轮执行时间">{firstTokenAt && <time dateTime={firstTokenAt}>{formatClock(firstTokenAt)}</time>}<span>{formatTurnDuration(elapsed)}</span></div>
}

function StepLabel({ title, icon }: { title: string; icon?: ReactNode }) {
  return <span className="step-label">
    {icon !== undefined && <span className="step-icon" aria-hidden="true">{icon}</span>}
    <span className="step-title">{title}</span>
  </span>
}

function ReasoningRow({ item, itemKey, onDisclosureChange }: Pick<Props, 'itemKey' | 'onDisclosureChange'> & { item: Extract<HistoryItem, { type: 'thinking' }> }) {
  const reduced = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const guard = useSelectionGuard(onDisclosureChange)
  const collapseBody = useBodyCollapse(expanded ? () => setExpanded(false) : undefined, onDisclosureChange)
  const running = item.status === 'running'
  const text = item.text.trim()
  // 思考中保持固定宽度，单行显示最新文字，新字从右侧进入、旧字在左侧淡出，块不随每行长短变宽变窄。
  const ticker = running && !expanded
  // 和工具卡片同一套展开方式：标题行不动，全文在下方展开；展开时标题行的摘要淡出，避免同一句话出现两次。
  return <motion.article {...enter} transition={reduced ? { duration: 0 } : pop} className={`timeline-item activity-step reasoning-row status-${item.status}${ticker ? ' is-streaming' : ''}`} data-item-id={itemKey}>
    <button type="button" className="activity-toggle" aria-expanded={expanded} {...guard(() => setExpanded(value => !value))}>
      <StepIcon name="thinking" status={item.status} />
      <span className={`step-title${running ? ' activity-shimmer' : ''}`}>{running ? '思考中' : '思考'}</span>
      <span className={`step-summary reasoning-summary${ticker ? ' is-ticker' : ''}`}>
        {ticker ? <span>{text.replace(/\s+/g, ' ').slice(-400)}</span> : text.split('\n')[0]}
      </span>
      <ExpandChevron expanded={expanded} size={13} />
    </button>
    <Disclosure open={expanded}><div className="activity-expanded reasoning-body" {...collapseBody}>{text}</div></Disclosure>
  </motion.article>
}

function CompactionRow({ item, itemKey, onDisclosureChange }: Pick<Props, 'itemKey' | 'onDisclosureChange'> & { item: Extract<HistoryItem, { type: 'compaction' | 'compaction_result' }> }) {
  const reduced = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const summary = item.type === 'compaction' ? item.summary : compactionResultText(item)
  const title = item.type === 'compaction' ? compactionText.completed : item.status === 'failed' ? compactionText.failed : summary
  const canExpand = summary !== '' && summary !== title
  const guard = useSelectionGuard(onDisclosureChange)
  const collapseBody = useBodyCollapse(expanded ? () => setExpanded(false) : undefined, onDisclosureChange)
  const status = item.type === 'compaction' ? 'completed' : item.status
  return <motion.article {...enter} transition={reduced ? { duration: 0 } : pop} className={`timeline-item activity-step timeline-compaction status-${status}`} data-item-id={itemKey}>
    <button type="button" className="activity-toggle compaction-toggle" aria-expanded={canExpand ? expanded : undefined} aria-disabled={!canExpand}
      {...guard(canExpand ? () => setExpanded(value => !value) : undefined)}>
      <StepIcon name="compaction" status={status} />
      <span className="compaction-label" role="status">{title}</span>
      {canExpand && <ExpandChevron expanded={expanded} size={14} />}
    </button>
    <Disclosure open={expanded}><div className="activity-expanded compaction-summary" {...collapseBody}><MarkdownBody text={summary} /></div></Disclosure>
  </motion.article>
}

function ToolOutput({ item: fact, filePath }: { item: Extract<HistoryItem, { type: 'tool' }>; filePath: string | null }) {
  const input = fact.args
  const background = ['bash', 'task_list', 'task_output', 'task_stop'].includes(fact.name)
  // 内部关联字段不展示；命令输出原样保留，不按工具参数过滤业务数据。
  const displayJson = (key: string, value: unknown) => background && (key === 'taskId' || key === 'task_id') ? undefined : value
  const parameters = JSON.stringify(input, (key, value: unknown) => fact.name === 'task_output' && key === 'cursor' ? undefined : displayJson(key, value), 2)
  let output = fact.result?.output ?? ''
  if (background && fact.name !== 'bash' && output !== '') {
    try { output = JSON.stringify(JSON.parse(output), displayJson, 2) }
    catch { /* 普通命令输出和工具错误是文本，原样显示。 */ }
  }
  const command = fact.name === 'bash' ? toolArgument(fact.name, input) : null
  if (command !== null) return <CommandOutput command={command} output={output} />
  const hasReadLines = fact.result?.readSource !== undefined && fact.result?.readSource.lineCount > 0
  if (filePath !== null && (output !== '' || hasReadLines) && fact.status !== 'failed') return <div>
    <div className="tool-output-header">{filePath}</div>
    {/* 后端提供 readSource 时为读取的正文行标上源文件行号。 */}
    {fact.result?.readSource
      ? <div className="tool-lines">{readOutputLines(output, fact.result.readSource).map((line, index) => (
        <div key={index} className="tool-line">{line.number !== undefined && <span className="tool-line-number">{line.number}</span>}<span>{line.text}</span></div>
      ))}</div>
      : <pre><code>{output}</code></pre>}
  </div>
  return (
    <div>
      {parameters && parameters !== '{}' && <ToolSection label="参数"><pre><code>{parameters}</code></pre></ToolSection>}
      {output !== '' && (
        <ToolSection label={fact.status === 'failed' ? '错误' : '输出'}>
          <pre><code>{output}</code></pre>
        </ToolSection>
      )}
    </div>
  )
}

function ToolSection({ label, children }: { label: string; children: ReactNode }) {
  return <section className="timeline-section">
    <h4>{label}</h4>
    {children}
  </section>
}

function StepIcon({ name, status }: { name: string; status: ToolStatus }) {
  if (status === 'running') return <LoaderCircle size={13} className="spinner" aria-hidden="true" />
  if (status === 'completed') return <CircleCheck size={13} className="ok" />
  if (status === 'failed') return <CircleAlert size={13} className="bad" />
  if (status === 'interrupted') return <CircleX size={13} className="bad" />
  const Icon = name === 'bash' ? SquareTerminal : name === 'ask_user_question' ? MessageCircleQuestion : name === 'view_image' ? Image : FileText
  return <Icon size={13} />
}

/** 截至这条回复时整个会话的累计：平均首 token 时间、平均输出速度、总用量和缓存命中率。 */
function MessageStatsLine({ stats, at }: { stats?: SessionModelUsage; at?: string }) {
  const parts: string[] = []
  if (stats && stats.ttftRequests > 0) parts.push(`TTFT ${(stats.ttftMs / stats.ttftRequests / 1000).toFixed(1)}s`)
  if (stats && stats.decodeMs > 0) parts.push(`${Math.round(stats.decodeTokens * 1000 / stats.decodeMs)} tok/s`)
  if (stats?.usagePresent) parts.push(`${formatTokenCount(stats.totalTokens)} tokens`)
  // 向下取整：只有全部命中时才显示 100%。
  if (stats?.usagePresent && stats.cacheUsageComplete && stats.inputTokens > 0) parts.push(`${Math.floor(stats.cachedInputTokens / stats.inputTokens * 100)}%`)
  if (at) parts.push(formatClock(at))
  // 文字只留数值，悬停时说明各项含义。
  return parts.length ? <p className="turn-stats" title="平均首 token 时间 · 平均输出速度 · 会话总用量 · 缓存命中率 · 时间">{parts.join(' · ')}</p> : null
}
