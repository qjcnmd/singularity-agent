import { ExpandChevron } from './ExpandChevron'
import { AttachedImages } from './Images'
import { Disclosure } from './Disclosure'
import { memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import Anser from 'anser'
import { CircleAlert } from 'lucide-react'
import { motion, useReducedMotion } from 'motion/react'
import { disclosureTransition } from '../motion'
import { useSelectionGuard } from '../interactions'
import { MarkdownBody } from '../markdown'
import { factStatusText } from '../copy'
import { readOutputLines } from '../readOutput'
import { formatTurnDuration, failureSummary, timelineBody, timelineStatus, toolArgument, type TimelineItemModel } from '../timeline'

const previewLineCount = 8

interface Props {
  item: TimelineItemModel
  sessionId?: string
}

/// 投影为未变化的项复用同一 item 引用；把该引用稳定性接到渲染边界，活动项的
/// 流式更新不会让整段历史 Markdown 重新渲染。展开等组件内状态不受影响。
export const TimelineItem = memo(function TimelineItem({ item, sessionId }: Props) {
  const isStep = stepKinds.has(item.kind)
  const hiddenLines = item.kind === 'user' ? Math.max(0, timelineBody(item).trimEnd().split('\n').length - previewLineCount) : 0
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(!isStep)
  const selectionGuard = useSelectionGuard()

  if (item.timing) return <TurnDuration itemKey={item.key} {...item.timing} />

  if (item.kind === 'terminal') return <span className="stopped-marker" data-item-id={item.key}>{item.title}</span>

  if (item.kind === 'user' || item.kind === 'assistant') {
    const body = canCollapse && !expanded ? preview(timelineBody(item)) : timelineBody(item)
    return (
      <article
        className={`timeline-item message-item timeline-${item.kind} status-${timelineStatus(item)}`}
        data-item-id={item.key}
        aria-label={`${item.title}，${factStatusText[timelineStatus(item)]}`}
      >
        <div className="timeline-body message-body">{item.kind === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} />}</div>
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

  const fact = item.fact
  const failure = timelineStatus(item) === 'error' ? (fact?.kind === 'tool' ? item.summary : failureSummary(fact?.error ?? timelineBody(item))) : undefined
  return (
    <article className={`timeline-item activity-step timeline-${item.kind} status-${timelineStatus(item)}`} data-item-id={item.key} aria-label={`${item.title}，${factStatusText[timelineStatus(item)]}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <StepLabel item={item} icon={<StepIcon item={item} />} />
        <ExpandChevron expanded={expanded} className="step-chevron" />
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{failure ?? oneLine(timelineBody(item))}</span>
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
  const animated = item.kind === 'thinking' || item.fact?.kind === 'tool'
  const muted = item.kind === 'thinking' || (item.fact?.kind === 'tool' && item.title === 'read')
  return <span className="step-label">
    {icon !== undefined && <span className="step-icon" aria-hidden="true">{icon}</span>}
    <span className={`step-title${animated ? ' execution-title' : ''}${muted ? ' muted-execution-title' : ''}`}>{item.title}</span>
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
  useEffect(() => {
    if (summaryRef.current !== null) summaryRef.current.scrollLeft = running && !expanded ? summaryRef.current.scrollWidth : 0
  }, [summary, running, expanded])
  const Row = canExpand ? motion.button : motion.div
  return <article className={`timeline-item reasoning-row status-${timelineStatus(item)}${showFullText ? ' is-expanded' : ''}`} data-item-id={item.key}>
    <Row initial={false} animate={{ height: expanded ? 'auto' : 24 }} transition={disclosureTransition(expanded, reducedMotion)} onAnimationComplete={() => { if (!expanded) setClosing(false) }} type={canExpand ? 'button' : undefined} className="activity-toggle" aria-expanded={canExpand ? expanded : undefined} {...(canExpand ? guard(() => { setClosing(expanded && !reducedMotion); setExpanded(value => !value) }) : {})}>
      <StepLabel item={item} />
      {canExpand ? <ExpandChevron expanded={expanded} className="step-chevron" /> : <span className="step-chevron" aria-hidden="true" />}
      <span className="step-separator" aria-hidden="true">·</span>
      <span className={`step-summary${running && !showFullText ? ' follows-end' : ''}`} ref={summaryRef}>
        {showFullText ? text : summary}
        <span className="reasoning-summary-measure" aria-hidden="true" ref={measureRef}>{summary}</span>
      </span>
      {running && <span className="sr-only">进行中</span>}
    </Row>
  </article>
}

function ToolOutput({ item }: Props) {
  const fact = item.fact
  if (fact?.kind !== 'tool') {
    const body = timelineBody(item)
    if (!body && !fact?.error) return null
    return (
      <div>
        {body && <ToolSection label="内容"><MarkdownBody text={body} /></ToolSection>}
        {fact?.error && <ToolSection label="错误"><pre><code>{fact.error}</code></pre></ToolSection>}
      </div>
    )
  }
  const { args: input, output } = fact
  const command = item.title === 'bash' ? toolArgument(item.title, input) : null
  if (command !== null) return <div>
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <><OutputHeader label="输出" /><pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{ color: part.fg ? `rgb(${part.fg})` : undefined, backgroundColor: part.bg ? `rgb(${part.bg})` : undefined, fontWeight: part.decorations.includes('bold') ? 700 : undefined }}>{part.content}</span>)}</pre></>}
  </div>
  const hasReadLines = fact.readSource !== undefined && fact.readSource.lineCount > 0
  if (item.filePath !== null && (output !== '' || hasReadLines) && item.title === 'read' && timelineStatus(item) !== 'error') return <div>
    <OutputHeader label={item.filePath} />
    {/* 只有 producer 记录了真实来源范围才编号；旧记录按普通文本展示，不猜边界。 */}
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

const stepKinds = new Set<TimelineItemModel['kind']>(['thinking', 'tool', 'compaction'])

function oneLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}



function StepIcon({ item }: Props) {
  if (timelineStatus(item) === 'error') return <CircleAlert size={15} strokeWidth={1.6} />
  const path = item.title === 'bash' ? 'm4 6 5 6-5 6m8 0h8'
    : 'M14 2H5v20h14V7zM14 2v6h5M8 12h8M8 16h8'
  return <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>
}
