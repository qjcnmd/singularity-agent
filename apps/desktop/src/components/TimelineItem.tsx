import { ExpandChevron } from './ExpandChevron'
import { AttachedImages } from './Images'
import { Disclosure } from './Disclosure'
import { memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { CommandOutput } from './CommandOutput'
import { CircleAlert, ListCollapse } from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'
import { disclosureTransition } from '../motion'
import { useSelectionGuard } from '../interactions'
import { MarkdownBody } from '../markdown'
import { compactionText, factStatusText } from '../copy'
import { readOutputLines } from '../readOutput'
import { compactionResultText, displayPath, firstLine, formatTurnDuration, pathFromArgs, toolSummary, toolArgument, type TimelineContent } from '../timeline'
import type { HistoryItem } from '../protocol'

const previewLineCount = 8

interface Props {
  item: TimelineContent
  itemKey: string
  cwd: string
  userHome?: string | null
  sessionId?: string
}

// 历史项复用稳定引用跳过重绘；流式更新只重新渲染当前活动项。
export const TimelineItem = memo(function TimelineItem({ item, itemKey, cwd, userHome, sessionId }: Props) {
  const userLines = item.type === 'message' && item.role === 'user' ? item.text.trimEnd().split('\n') : []
  const hiddenLines = Math.max(0, userLines.length - previewLineCount)
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(item.type !== 'tool')
  const selectionGuard = useSelectionGuard()

  if (item.type === 'duration') return <TurnDuration itemKey={itemKey} {...item} />

  if (item.type === 'terminal') {
    if (item.status === 'failed') return <div className="turn-error" data-item-id={itemKey} role="alert">
      <CircleAlert size={15} strokeWidth={1.6} aria-hidden="true" /><span>{item.text}</span>
    </div>
    return <span className="stopped-marker" data-item-id={itemKey}>{item.text}</span>
  }

  if (item.type === 'message') {
    const body = canCollapse && !expanded ? userLines.slice(0, previewLineCount).join('\n') : item.text
    return (
      <article
        className={`timeline-item message-item timeline-${item.role} status-${item.status}`}
        data-item-id={itemKey}
        aria-label={`${item.role === 'user' ? '你' : 'Singularity'}，${factStatusText[item.status]}`}
      >
        <div className="timeline-body message-body">{item.role === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} streaming={item.status === 'running'} />}</div>
        {sessionId && item.images && <AttachedImages sessionId={sessionId} images={item.images} />}
        {canCollapse && (
          <button type="button" className="expand-button" aria-expanded={expanded} {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${hiddenLines} 行`}
          </button>
        )}
      </article>
    )
  }

  if (item.type === 'thinking') return <ReasoningRow item={item} itemKey={itemKey} />
  if (item.type === 'compaction' || item.type === 'compaction_result') return <CompactionRow item={item} itemKey={itemKey} />

  const filePath = pathFromArgs(item.name, item.args)
  const title = item.name === 'ask_user_question' ? '询问用户' : filePath?.replace(/\\/g, '/').endsWith('/SKILL.md') ? '读取技能' : item.name
  const summary = filePath !== null ? displayPath(filePath, cwd, userHome) : toolSummary(item.name, item.args) || firstLine(item.result?.output ?? '')

  return (
    <article className={`timeline-item activity-step timeline-tool status-${item.status}`} data-item-id={itemKey} aria-label={`${title}，${factStatusText[item.status]}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <StepLabel title={title} muted={title === 'read'} icon={<StepIcon name={item.name} />} />
        <ExpandChevron expanded={expanded} className="step-chevron" />
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{summary}</span>
        {item.status === 'interrupted' && <span className="item-status">{factStatusText.interrupted}</span>}
        {item.status === 'pending' && <span className="item-status">{factStatusText.pending}</span>}
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        {item.result?.durationMs !== undefined && <div className="tool-duration">耗时 {formatTurnDuration(item.result.durationMs)}</div>}
        <div className="timeline-body activity-output"><ToolOutput item={item} filePath={filePath} />{sessionId && item.result?.images && <AttachedImages sessionId={sessionId} images={item.result.images} />}</div>
      </div></Disclosure>
    </article>
  )
})

function TurnDuration({ itemKey, startedAt, finishedAt }: { itemKey: string; startedAt: string; finishedAt?: string }) {
  const [now, setNow] = useState(Date.now)
  useEffect(() => {
    if (finishedAt) return
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [finishedAt])
  const elapsed = (finishedAt ? Date.parse(finishedAt) : now) - Date.parse(startedAt)
  return <div className="turn-duration" data-item-id={itemKey} role={finishedAt ? undefined : 'timer'} aria-label="本轮执行时间">{finishedAt ? '本轮耗时' : '执行中'} · {formatTurnDuration(elapsed)}</div>
}

function StepLabel({ title, muted = false, icon }: { title: string; muted?: boolean; icon?: ReactNode }) {
  return <span className="step-label">
    {icon !== undefined && <span className="step-icon" aria-hidden="true">{icon}</span>}
    <span className={`step-title execution-title${muted ? ' muted-execution-title' : ''}`}>{title}</span>
  </span>
}

function ReasoningRow({ item, itemKey }: { item: Extract<HistoryItem, { type: 'thinking' }>; itemKey: string }) {
  const reducedMotion = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const [closing, setClosing] = useState(false)
  const showFullText = expanded || closing
  const [canExpand, setCanExpand] = useState(false)
  const summaryRef = useRef<HTMLSpanElement>(null)
  const measureRef = useRef<HTMLSpanElement>(null)
  const guard = useSelectionGuard()
  const running = item.status === 'running'
  const text = item.text.trim().replace(/\n[\t \r]*\n+/g, '\n')
  const summary = running ? text.slice(text.lastIndexOf('\n') + 1) : text.split('\n')[0]
  useLayoutEffect(() => {
    const node = summaryRef.current, measure = measureRef.current
    if (!node || !measure) return
    const update = () => {
      if (showFullText) return
      setCanExpand(text.includes('\n') || measure.getBoundingClientRect().width > node.clientWidth + 1)
    }
    update()
    const observer = new ResizeObserver(update)
    observer.observe(node)
    observer.observe(measure)
    return () => observer.disconnect()
  }, [text, summary, showFullText])
  useLayoutEffect(() => {
    const node = summaryRef.current
    if (!node) return
    if (showFullText || !running) { node.scrollLeft = 0; return }
    const start = node.scrollLeft
    const end = Math.max(0, node.scrollWidth - node.clientWidth)
    if (reducedMotion || end <= start) { node.scrollLeft = end; return }
    const started = performance.now()
    let frame: number
    const follow = (now: number) => {
      const progress = Math.min(1, (now - started) / 80)
      node.scrollLeft = start + (end - start) * (1 - (1 - progress) ** 3)
      if (progress < 1) frame = requestAnimationFrame(follow)
    }
    frame = requestAnimationFrame(follow)
    return () => cancelAnimationFrame(frame)
  }, [summary, running, showFullText, reducedMotion])
  return <article className={`timeline-item reasoning-row status-${item.status}${showFullText ? ' is-expanded' : ''}`} data-item-id={itemKey}>
    {/* 保持同一个节点，避免溢出状态切换时替换控件、重启光泽动画和丢失测量对象。 */}
    <motion.button initial={false} animate={{ height: expanded ? 'auto' : 24 }} transition={disclosureTransition(expanded, reducedMotion)} onAnimationComplete={() => { if (!expanded) setClosing(false) }} type="button" className="activity-toggle" tabIndex={canExpand ? 0 : -1} aria-disabled={!canExpand} aria-expanded={canExpand ? expanded : undefined} {...guard(() => {
      if (!canExpand) return
      setClosing(expanded && !reducedMotion)
      setExpanded(value => !value)
    })}>
      <StepLabel title="思考" muted />
      {canExpand ? <ExpandChevron expanded={expanded} className="step-chevron" /> : <span className="step-chevron" aria-hidden="true" />}
      <span className="step-separator" aria-hidden="true">·</span>
      <span className={`step-summary${running && !showFullText ? ' follows-end' : ''}`} ref={summaryRef}>
        {showFullText ? text : summary}
        <span className="reasoning-summary-measure" aria-hidden="true" ref={measureRef}>{summary}</span>
      </span>
      {running && <span className="sr-only">进行中</span>}
    </motion.button>
  </article>
}

function CompactionRow({ item, itemKey }: { item: Extract<HistoryItem, { type: 'compaction' | 'compaction_result' }>; itemKey: string }) {
  const [expanded, setExpanded] = useState(false)
  const summary = item.type === 'compaction' ? item.summary : compactionResultText(item)
  const title = item.type === 'compaction' ? compactionText.completed : item.status === 'failed' ? compactionText.failed : summary
  const canExpand = summary !== '' && summary !== title
  const guard = useSelectionGuard()
  const status = item.type === 'compaction' ? 'recorded' : item.status
  return <article className={`timeline-item activity-step timeline-compaction status-${status}`} data-item-id={itemKey}>
    <button type="button" className="activity-toggle compaction-toggle" aria-expanded={canExpand ? expanded : undefined} aria-disabled={!canExpand}
      {...guard(() => { if (canExpand) setExpanded(value => !value) })}>
      <ListCollapse size={16} strokeWidth={1.6} aria-hidden="true" />
      <span className="compaction-label" role="status">{title}</span>
      {canExpand && <ExpandChevron expanded={expanded} size={14} />}
    </button>
    <Disclosure open={expanded}><div className="activity-expanded compaction-summary"><MarkdownBody text={summary} /></div></Disclosure>
  </article>
}

function ToolOutput({ item: fact, filePath }: { item: Extract<HistoryItem, { type: 'tool' }>; filePath: string | null }) {
  const input = fact.args
  const output = fact.result?.output ?? ''
  const command = fact.name === 'bash' ? toolArgument(fact.name, input) : null
  if (command !== null) return <CommandOutput command={command} output={output} />
  const hasReadLines = fact.result?.readSource !== undefined && fact.result?.readSource.lineCount > 0
  if (filePath !== null && (output !== '' || hasReadLines) && fact.name === 'read' && fact.status !== 'failed') return <div>
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
      <ToolSection label="参数"><pre><code>{JSON.stringify(input, null, 2) || '（空）'}</code></pre></ToolSection>
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

function StepIcon({ name }: { name: string }) {
  const path = name === 'bash' ? 'm4 6 5 6-5 6m8 0h8'
    : 'M14 2H5v20h14V7zM14 2v6h5M8 12h8M8 16h8'
  return <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>
}
