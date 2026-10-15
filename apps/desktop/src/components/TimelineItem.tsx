import { ExpandChevron } from './ExpandChevron'
import { AttachedImages } from './Images'
import { Disclosure } from './Disclosure'
import { memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import Anser from 'anser'
import { CircleAlert, ListCollapse } from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'
import { disclosureTransition } from '../motion'
import { useSelectionGuard } from '../interactions'
import { MarkdownBody } from '../markdown'
import { factStatusText } from '../copy'
import { readOutputLines } from '../readOutput'
import { formatTurnDuration, timelineBody, timelineStatus, toolArgument, type TimelineItemModel } from '../timeline'

const previewLineCount = 8

interface Props {
  item: TimelineItemModel
  sessionId?: string
}

// 性能优化：历史项复用稳定引用跳过重绘，流式更新时仅重新渲染当前活动项。
export const TimelineItem = memo(function TimelineItem({ item, sessionId }: Props) {
  const hiddenLines = item.kind === 'user' ? Math.max(0, timelineBody(item).trimEnd().split('\n').length - previewLineCount) : 0
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(item.kind !== 'tool')
  const selectionGuard = useSelectionGuard()

  if (item.kind === 'duration') return <TurnDuration itemKey={item.key} {...item.timing} />

  if (item.kind === 'terminal') {
    if (timelineStatus(item) === 'error') return <div className="turn-error" data-item-id={item.key} role="alert">
      <CircleAlert size={15} strokeWidth={1.6} aria-hidden="true" /><span>{item.summary}</span>
    </div>
    return <span className="stopped-marker" data-item-id={item.key}>{item.title}</span>
  }

  if (item.kind === 'user' || item.kind === 'assistant') {
    const body = canCollapse && !expanded ? preview(timelineBody(item)) : timelineBody(item)
    return (
      <article
        className={`timeline-item message-item timeline-${item.kind} status-${timelineStatus(item)}`}
        data-item-id={item.key}
        aria-label={`${item.title}，${factStatusText[timelineStatus(item)]}`}
      >
        <div className="timeline-body message-body">{item.kind === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} streaming={timelineStatus(item) === 'running'} />}</div>
        {sessionId && item.fact?.images && <AttachedImages sessionId={sessionId} images={item.fact.images} />}
        {canCollapse && (
          <button type="button" className="expand-button" aria-expanded={expanded} {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${hiddenLines} 行`}
          </button>
        )}
      </article>
    )
  }

  if (item.kind === 'thinking') return <ReasoningRow item={item} />
  if (item.kind === 'compaction') return <CompactionRow item={item} />

  return (
    <article className={`timeline-item activity-step timeline-${item.kind} status-${timelineStatus(item)}`} data-item-id={item.key} aria-label={`${item.title}，${factStatusText[timelineStatus(item)]}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <StepLabel item={item} icon={<StepIcon item={item} />} />
        <ExpandChevron expanded={expanded} className="step-chevron" />
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{item.summary}</span>
        {timelineStatus(item) === 'cancelled' && <span className="item-status">{factStatusText.cancelled}</span>}
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        <div className="timeline-body activity-output"><ToolOutput item={item} />{sessionId && item.fact?.images && <AttachedImages sessionId={sessionId} images={item.fact.images} />}</div>
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
  return <div className="turn-duration" data-item-id={itemKey} role={finishedAt ? undefined : 'timer'} aria-label="本轮执行时间">{formatTurnDuration(elapsed)}</div>
}

function StepLabel({ item, icon }: Props & { icon?: ReactNode }) {
  const muted = item.kind === 'thinking' || (item.fact?.kind === 'tool' && item.title === 'read')
  return <span className="step-label">
    {icon !== undefined && <span className="step-icon" aria-hidden="true">{icon}</span>}
    <span className={`step-title execution-title${muted ? ' muted-execution-title' : ''}`}>{item.title}</span>
  </span>
}

function ReasoningRow({ item }: Props) {
  const reducedMotion = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const [closing, setClosing] = useState(false)
  const showFullText = expanded || closing
  const [canExpand, setCanExpand] = useState(false)
  const summaryRef = useRef<HTMLSpanElement>(null)
  const measureRef = useRef<HTMLSpanElement>(null)
  const guard = useSelectionGuard()
  const running = timelineStatus(item) === 'running'
  const text = timelineBody(item).trim().replace(/\n[\t \r]*\n+/g, '\n')
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
  return <article className={`timeline-item reasoning-row status-${timelineStatus(item)}${showFullText ? ' is-expanded' : ''}`} data-item-id={item.key}>
    {/* 保持同一个节点，避免溢出状态切换时替换控件、重启光泽动画和丢失测量对象。 */}
    <motion.button initial={false} animate={{ height: expanded ? 'auto' : 24 }} transition={disclosureTransition(expanded, reducedMotion)} onAnimationComplete={() => { if (!expanded) setClosing(false) }} type="button" className="activity-toggle" tabIndex={canExpand ? 0 : -1} aria-disabled={!canExpand} aria-expanded={canExpand ? expanded : undefined} {...guard(() => {
      if (!canExpand) return
      setClosing(expanded && !reducedMotion)
      setExpanded(value => !value)
    })}>
      <StepLabel item={item} />
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

function CompactionRow({ item }: Props) {
  const [expanded, setExpanded] = useState(false)
  const summary = timelineBody(item)
  const canExpand = summary !== '' && summary !== item.title
  const guard = useSelectionGuard()
  const status = timelineStatus(item)
  return <article className={`timeline-item activity-step timeline-compaction status-${status}`} data-item-id={item.key}>
    <button type="button" className="activity-toggle compaction-toggle" aria-expanded={canExpand ? expanded : undefined} aria-disabled={!canExpand}
      {...guard(() => { if (canExpand) setExpanded(value => !value) })}>
      <ListCollapse size={16} strokeWidth={1.6} aria-hidden="true" />
      <span className="compaction-label" role="status">{item.title}</span>
      {canExpand && <ExpandChevron expanded={expanded} size={14} />}
    </button>
    <Disclosure open={expanded}><div className="activity-expanded compaction-summary"><MarkdownBody text={summary} /></div></Disclosure>
  </article>
}

function ToolOutput({ item }: { item: Extract<TimelineItemModel, { kind: 'tool' }> }) {
  const fact = item.fact
  const { args: input, output } = fact
  const command = item.title === 'bash' ? toolArgument(item.title, input) : null
  if (command !== null) return <div>
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <><OutputHeader label="输出" /><pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{ color: part.fg ? `rgb(${part.fg})` : undefined, backgroundColor: part.bg ? `rgb(${part.bg})` : undefined, fontWeight: part.decorations.includes('bold') ? 700 : undefined }}>{part.content}</span>)}</pre></>}
  </div>
  const hasReadLines = fact.readSource !== undefined && fact.readSource.lineCount > 0
  if (item.filePath !== null && (output !== '' || hasReadLines) && item.title === 'read' && timelineStatus(item) !== 'error') return <div>
    <OutputHeader label={item.filePath} />
    {/* 只有 producer 记录了真实来源范围才编号；否则按普通文本展示，不猜边界。 */}
    {fact.readSource
      ? <div className="tool-lines">{readOutputLines(output, fact.readSource).map((line, index) => (
        <div key={index} className="tool-line">{line.number !== undefined && <span className="tool-line-number">{line.number}</span>}<span>{line.text}</span></div>
      ))}</div>
      : <pre><code>{output}</code></pre>}
  </div>
  return (
    <div>
      <ToolSection label="参数"><pre><code>{JSON.stringify(input, null, 2) || '（空）'}</code></pre></ToolSection>
      {output !== '' && (
        <ToolSection label={timelineStatus(item) === 'error' ? '错误' : '输出'}>
          <pre><code>{output}</code></pre>
        </ToolSection>
      )}
    </div>
  )
}

function OutputHeader({ label }: { label: string }) {
  return <div className="tool-output-header">{label}</div>
}

function ToolSection({ label, children }: { label: string; children: ReactNode }) {
  return <section className="timeline-section">
    <h4>{label}</h4>
    {children}
  </section>
}

function preview(text: string): string {
  return text.split('\n').slice(0, previewLineCount).join('\n')
}

function StepIcon({ item }: Props) {
  const path = item.title === 'bash' ? 'm4 6 5 6-5 6m8 0h8'
    : 'M14 2H5v20h14V7zM14 2v6h5M8 12h8M8 16h8'
  return <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>
}
