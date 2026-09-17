import { ExpandChevron } from './ExpandChevron'
import { Disclosure } from './Disclosure'
import { memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import Anser from 'anser'
import type { StructuredPatch } from 'diff'
import { Pencil } from 'lucide-react'
import { diffContext } from '../diffView'
import { motion, useReducedMotion } from 'motion/react'
import { disclosureTransition } from '../motion'
import { useSelectionGuard } from '../interactions'
import { CodeTokens, MarkdownBody, useCodeTokens } from '../markdown'
import { factStatusText } from '../copy'
import { readOutputLines } from '../readOutput'
import { timelineBody, timelineStatus, type TimelineItemModel } from '../timeline'

const previewLineCount = 8

interface Props {
  item: TimelineItemModel
}

/// 投影为未变化的项复用同一 item 引用；这里把该引用稳定性接到渲染边界上，使
/// 活动项的流式更新不再让整段历史 Markdown 重新渲染。展开等组件内状态不受影响。
export const TimelineItem = memo(function TimelineItem({ item }: Props) {
  const isStep = stepKinds.has(item.kind)
  const hiddenLines = item.kind === 'user' ? Math.max(0, timelineBody(item).trimEnd().split('\n').length - previewLineCount) : 0
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(!isStep)
  const selectionGuard = useSelectionGuard()

  if (item.kind === 'terminal') return <span className="stopped-marker" data-item-id={item.key}>已停止</span>

  if (item.kind === 'user' || item.kind === 'assistant') {
    const body = canCollapse && !expanded ? preview(timelineBody(item)) : timelineBody(item)
    return (
      <article
        className={`timeline-item message-item timeline-${item.kind} status-${timelineStatus(item)}`}
        data-item-id={item.key}
        aria-label={`${item.title}，${statusLabel(timelineStatus(item)) || factStatusText.stable}`}
      >
        <div className="timeline-body message-body">{item.kind === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} />}</div>
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
  const failure = timelineStatus(item) === 'error' ? (fact?.kind === 'tool' ? fact.output : fact?.error)?.split('\n')[0] : undefined
  return (
    <article className={`timeline-item activity-step timeline-${item.kind} status-${timelineStatus(item)}`} data-item-id={item.key} aria-label={`${item.title}，${statusLabel(timelineStatus(item)) || factStatusText.stable}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <StepLabel item={item} icon={<StepIcon item={item} />} />
        <ExpandChevron expanded={expanded} className="step-chevron" />
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{failure ?? oneLine(timelineBody(item))}</span>
        {item.addedLines > 0 && <span className="diff-stat is-added">+{item.addedLines}</span>}
        {item.removedLines > 0 && <span className="diff-stat is-removed">−{item.removedLines}</span>}
        {['error', 'cancelled'].includes(timelineStatus(item)) && <span className="item-status">{statusLabel(timelineStatus(item))}</span>}
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        <div className="timeline-body activity-output"><ToolOutput item={item} /></div>
      </div></Disclosure>
    </article>
  )
})

function StepLabel({ item, icon }: Props & { icon?: ReactNode }) {
  const animated = item.kind === 'thinking' || item.tool !== undefined
  const muted = item.kind === 'thinking' || (item.tool !== undefined && item.title === 'read')
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
  const tool = item.tool
  if (!tool || fact?.kind !== 'tool') {
    const body = timelineBody(item)
    const sections: TimelineSection[] = []
    if (body) sections.push({ label: '内容', content: body, kind: 'text' })
    if (fact?.error) sections.push({ label: '错误', content: fact.error, kind: 'error' })
    return <SectionList sections={sections} />
  }
  const { args: input, output } = fact
  const { diff, patches } = tool
  if (diff !== '') return <DiffBody text={diff} patches={patches} />
  const args = typeof input === 'object' && input !== null ? input as Record<string, unknown> : {}
  const command = item.title === 'bash' && typeof args.command === 'string' ? args.command : null
  if (command !== null) return <div className="terminal-output">
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <><OutputHeader label="输出" /><pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{ color: part.fg ? `rgb(${part.fg})` : undefined, backgroundColor: part.bg ? `rgb(${part.bg})` : undefined, fontWeight: part.decorations.includes('bold') ? 700 : undefined }}>{part.content}</span>)}</pre></>}
  </div>
  if (item.filePath !== null && output !== '' && item.title === 'read' && timelineStatus(item) !== 'error') return <div className="file-output">
    <OutputHeader label={item.filePath} /><NumberedOutput text={output} startLine={typeof args.offset === 'number' ? args.offset : 1} />
  </div>
  if (output !== '' && (item.title === 'grep' || item.title === 'glob')) return <div className="file-output"><OutputHeader label="搜索结果" /><NumberedOutput text={output} /></div>
  const sections: TimelineSection[] = [{ label: '参数', content: JSON.stringify(input, null, 2), kind: 'json' }]
  if (output !== '') sections.push({ label: timelineStatus(item) === 'error' ? '错误' : '输出', content: output, kind: timelineStatus(item) === 'error' ? 'error' : 'code' })
  return <SectionList sections={sections} />
}

function OutputHeader({ label }: { label: string }) {
  return <div className="tool-output-header">{label}</div>
}

function NumberedOutput({ text, startLine }: { text: string; startLine?: number }) {
  // read 的编号以 offset 为准，只剔除后端在末尾追加的完整说明。
  if (startLine !== undefined) return <div className="tool-lines">{readOutputLines(text, startLine).map((line, index) => (
    <div key={index} className="tool-line">{line.number !== undefined && <span className="tool-line-number">{line.number}</span>}<span>{line.text}</span></div>
  ))}</div>
  // grep/glob 的 file:line: 前缀仍按原有规则解析。
  const lines = text.replace(/\r\n/g, '\n').replace(/\n$/, '').split('\n')
  return <div className="tool-lines">{lines.map((line, index) => {
    const match = /^(.*?):(\d+):(.*)$/.exec(line)
    return <div key={index} className="tool-line">{match && <span className="search-file">{match[1]}:</span>}{match?.[2] !== undefined && <span className="tool-line-number">{match[2]}</span>}<span>{match?.[3] ?? line}</span></div>
  })}</div>
}

/** 工具详情的私有呈现配置：只由本文件的 SectionList 构造和渲染。 */
interface TimelineSection {
  label: string
  content: string
  kind: 'text' | 'code' | 'error' | 'json'
}

/// 工具详情只渲染当前真实来源的 section；没有内容时不占位。
function SectionList({ sections }: { sections: TimelineSection[] }) {
  if (sections.length === 0) return null
  return (
    <div className="timeline-sections">
      {sections.map((section, index) => (
        <section className={`timeline-section section-${section.kind}`} key={`${section.label}:${index}`}>
          <h4>{section.label}</h4>
          {section.kind === 'text'
              ? <MarkdownBody text={section.content} />
              : <pre><code>{section.content || '（空）'}</code></pre>}
        </section>
      ))}
    </div>
  )
}

function DiffBody({ text, patches }: { text: string; patches: StructuredPatch[] }) {
  if (patches.length === 0) return <div className="file-output"><OutputHeader label="文件改动" /><pre>{text}</pre></div>
  return <div className="diff-files">{patches.map((patch, index) => <DiffFile key={index} patch={patch} />)}</div>
}

function DiffFile({ patch }: { patch: StructuredPatch }) {
  const filename = (patch.newFileName === '/dev/null' ? patch.oldFileName : patch.newFileName) ?? ''
  const extension = filename.split('.').pop()?.toLowerCase() ?? ''
  const language = ({ js: 'javascript', jsx: 'javascript', ts: 'typescript', tsx: 'tsx', rs: 'rust', json: 'json', md: 'markdown', sh: 'bash' } as Record<string, string>)[extension] ?? 'text'
  return <section className="diff-file">
    <div className="diff-file-lines">{diffContext(patch.hunks).map((hunk, index) => <DiffHunk key={index} hunk={hunk} language={language} />)}</div>
  </section>
}

function DiffHunk({ hunk, language }: { hunk: StructuredPatch['hunks'][number]; language: string }) {
  const before = useCodeTokens(hunk.lines.filter(line => line[0] === '-' || line[0] === ' ').map(line => line.slice(1)).join('\n'), language)
  const after = useCodeTokens(hunk.lines.filter(line => line[0] === '+' || line[0] === ' ').map(line => line.slice(1)).join('\n'), language)
  let oldLine = hunk.oldStart, newLine = hunk.newStart, beforeIndex = 0, afterIndex = 0
  return <div className="diff-hunk">{hunk.lines.map((line, index) => {
    const marker = line[0]
    if (marker === '\\') return <div className="diff-no-newline" key={index}>{line.slice(2)}</div>
    const removed = marker === '-', added = marker === '+'
    const number = removed ? oldLine : newLine
    const tokens = removed ? before?.[beforeIndex] : after?.[afterIndex]
    if (!added) { oldLine++; beforeIndex++ }
    if (!removed) { newLine++; afterIndex++ }
    return <div key={index} className={`diff-line ${added ? 'diff-add' : removed ? 'diff-remove' : 'diff-context'}`} aria-label={added ? `新增行 ${number}` : removed ? `删除行 ${number}` : undefined}><span className="diff-line-number">{number}</span><code><CodeTokens tokens={tokens} fallback={line.slice(1)} /></code></div>
  })}</div>
}

function preview(text: string): string {
  return text.split('\n').slice(0, previewLineCount).join('\n')
}

/// 时间线只在非 stable 状态显示标签；词表与轨迹共用 copy.ts 的一份。
function statusLabel(status: ReturnType<typeof timelineStatus>): string {
  return status === 'stable' ? '' : factStatusText[status]
}

const stepKinds = new Set<TimelineItemModel['kind']>(['thinking', 'tool', 'diff', 'diagnostic', 'unknown'])

function oneLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}



function StepIcon({ item }: Props) {
  if (item.kind === 'diff') return <Pencil size={16} strokeWidth={1.6} aria-hidden="true" />
  const path = item.title === 'bash' ? 'm4 6 5 6-5 6m8 0h8'
    : item.title === 'grep' || item.title === 'glob' ? 'M15 15l6 6M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0'
      : 'M14 2H5v20h14V7zM14 2v6h5M8 12h8M8 16h8'
  return <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>
}
