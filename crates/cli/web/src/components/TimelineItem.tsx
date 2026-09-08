import { CopyButton } from './CopyButton'
import { ExpandChevron } from './ExpandChevron'
import { Disclosure } from './Disclosure'
import { useEffect, useLayoutEffect, useRef, useState, isValidElement, type CSSProperties, type ReactNode } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import Anser from 'anser'
import { parsePatch } from 'diff'
import 'katex/dist/katex.min.css'
import { motion, useReducedMotion } from 'motion/react'
import { highlightCode } from '../highlight'
import { useSelectionGuard } from '../interactions'
import type { TimelineItemModel, TimelineSection } from '../timeline'

const previewLineCount = 8

interface Props {
  item: TimelineItemModel
}

export function TimelineItem({ item }: Props) {
  const isStep = stepKinds.has(item.kind)
  const hiddenLines = item.kind === 'user' ? Math.max(0, item.body.trimEnd().split('\n').length - previewLineCount) : 0
  const canCollapse = hiddenLines > 0
  const [expanded, setExpanded] = useState(!isStep)
  const selectionGuard = useSelectionGuard()

  if (item.kind === 'terminal') return <span className="stopped-marker" data-item-id={item.key}>已停止</span>

  if (item.kind === 'user' || item.kind === 'assistant') {
    const body = canCollapse && !expanded ? preview(item.body) : item.body
    return (
      <article
        className={`timeline-item message-item timeline-${item.kind} status-${item.status}`}
        data-item-id={item.key}
        aria-label={`${item.title}，${statusLabel(item.status) || '已记录'}`}
      >
        <div className="timeline-body message-body">{item.kind === 'user' ? <div className="user-text">{body}</div> : <MarkdownBody text={body} />}</div>
        {canCollapse && (
          <button type="button" className="expand-button" {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${hiddenLines} 行`}
          </button>
        )}
      </article>
    )
  }

  if (item.kind === 'thinking') return <ReasoningRow item={item} />

  const failure = item.status === 'failed' ? (item.tool?.output ?? item.sections.find(section => section.kind === 'error')?.content)?.split('\n')[0] : undefined
  return (
    <article className={`timeline-item activity-step timeline-${item.kind} status-${item.status}`} data-item-id={item.key} aria-label={`${item.title}，${statusLabel(item.status) || '已记录'}`}>
      <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded(value => !value))} aria-expanded={expanded}>
        <span className="disclosure-leading" aria-hidden="true"><span className="step-icon"><StepIcon item={item} /></span><ExpandChevron expanded={expanded} className={`step-chevron${expanded ? ' is-open' : ''}`} /></span>
        <span className="step-title">{item.title}</span>
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{failure ?? oneLine(item.body)}</span>
        {item.addedLines > 0 && <span className="diff-stat is-added">+{item.addedLines}</span>}
        {item.removedLines > 0 && <span className="diff-stat is-removed">−{item.removedLines}</span>}
        {['running', 'failed', 'interrupted'].includes(item.status) && <span className="item-status">{statusLabel(item.status)}</span>}
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        <div className="timeline-body activity-output"><ToolOutput item={item} /></div>
      </div></Disclosure>
    </article>
  )
}

function ReasoningRow({ item }: Props) {
  const reducedMotion = useReducedMotion()
  const [expanded, setExpanded] = useState(false)
  const [closing, setClosing] = useState(false)
  const fullWidth = expanded || closing
  const [canExpand, setCanExpand] = useState(false)
  const summaryRef = useRef<HTMLSpanElement>(null)
  const measureRef = useRef<HTMLSpanElement>(null)
  const guard = useSelectionGuard()
  const running = item.status === 'running'
  const text = item.body.trimEnd()
  const summary = running ? text.slice(text.lastIndexOf('\n') + 1) : text.split('\n')[0]
  useLayoutEffect(() => {
    const node = summaryRef.current, measure = measureRef.current
    if (!node || !measure) return
    const update = () => {
      if (fullWidth) return
      const chevron = node.parentElement?.querySelector<SVGElement>('.step-chevron')
      const gap = Number.parseFloat(getComputedStyle(node.parentElement!).columnGap) || 0
      const available = node.clientWidth + (chevron ? chevron.getBoundingClientRect().width + (Number.parseFloat(getComputedStyle(chevron).marginLeft) || 0) + gap : 0)
      const overflow = text.includes('\n') || measure.getBoundingClientRect().width > available + 1
      setCanExpand(overflow)
      if (!overflow) setExpanded(false)
    }
    update()
    const observer = new ResizeObserver(update)
    observer.observe(node)
    observer.observe(measure)
    return () => observer.disconnect()
  }, [text, summary, canExpand, fullWidth])
  useEffect(() => {
    if (summaryRef.current !== null) summaryRef.current.scrollLeft = running && !expanded ? summaryRef.current.scrollWidth : 0
  }, [summary, running, expanded])
  const Row = canExpand ? 'button' : 'div'
  return <article className={`timeline-item reasoning-row status-${item.status}${fullWidth ? ' is-expanded' : ''}`} data-item-id={item.key}>
    <Row type={canExpand ? 'button' : undefined} className="activity-toggle" aria-expanded={canExpand ? expanded : undefined} {...(canExpand ? guard(() => { setClosing(expanded && !reducedMotion); setExpanded(value => !value) }) : {})}>
      <span className="step-title">{item.title}</span>
      {canExpand && <ExpandChevron expanded={expanded} className={`step-chevron${expanded ? ' is-open' : ''}`} />}
      <span className="step-separator" aria-hidden="true">·</span>
      <motion.span className={`step-summary${running && !fullWidth ? ' follows-end' : ''}`} ref={summaryRef} initial={false} animate={{ height: expanded ? 'auto' : closing ? 0 : 24, marginTop: expanded ? 6 : 0 }} transition={{ duration: reducedMotion ? 0 : 0.28, ease: [0.2, 0.8, 0.2, 1] }} onAnimationComplete={() => { if (!expanded) setClosing(false) }}>
        {fullWidth ? text : summary}
        <span className="reasoning-summary-measure" aria-hidden="true" ref={measureRef}>{summary}</span>
      </motion.span>
      {running && <span className="sr-only">进行中</span>}
    </Row>
  </article>
}

function ToolOutput({ item }: Props) {
  if (!item.tool) return <SectionList sections={item.sections} fallback={item.body} />
  const { args: input, output, diff } = item.tool
  if (diff !== '') return <DiffBody text={diff} />
  const args = typeof input === 'object' && input !== null ? input as Record<string, unknown> : {}
  const command = typeof args.command === 'string' ? args.command : typeof args.cmd === 'string' ? args.cmd : null
  if (command !== null) return <div className="terminal-output">
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <><OutputHeader text={output} label="输出" /><pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{ color: part.fg ? `rgb(${part.fg})` : undefined, backgroundColor: part.bg ? `rgb(${part.bg})` : undefined, fontWeight: part.decorations.includes('bold') ? 700 : undefined }}>{part.content}</span>)}</pre></>}
  </div>
  if (item.filePath !== null && output !== '' && item.title === 'read' && item.status !== 'failed') return <div className="file-output">
    <OutputHeader text={output} label={item.filePath} /><NumberedOutput text={output} startLine={typeof args.offset === 'number' ? args.offset : 1} />
  </div>
  if (output !== '' && (item.title.toLowerCase() === 'grep' || item.title.toLowerCase() === 'glob')) return <div className="file-output"><OutputHeader text={output} label="搜索结果" /><NumberedOutput text={output} /></div>
  const sections: TimelineSection[] = [{ label: '参数', content: JSON.stringify(input, null, 2), kind: 'json' }]
  if (output !== '') sections.push({ label: item.status === 'failed' ? '错误' : '输出', content: output, kind: item.status === 'failed' ? 'error' : 'code' })
  return <SectionList sections={sections} fallback={item.body} />
}

function OutputHeader({ text, label }: { text: string; label: string }) {
  return <div className="tool-copy-header"><span>{label}</span><CopyButton text={text} label="复制输出" /></div>
}

function NumberedOutput({ text, startLine }: { text: string; startLine?: number }) {
  const lines = text.replace(/\r\n/g, '\n').replace(/\n$/, '').split('\n')
  const footer = lines.findIndex(line => line.startsWith('[Showing lines '))
  return <div className="tool-lines">{lines.map((line, index) => {
    const match = startLine === undefined ? /^(.*?):(\d+):(.*)$/.exec(line) : null
    const number = startLine !== undefined && (footer < 0 || index < footer - 1) ? startLine + index : match?.[2]
    return <div key={index} className="tool-line">{match && <span className="search-file">{match[1]}:</span>}{number !== undefined && <span className="tool-line-number">{number}</span>}<span>{match?.[3] ?? line}</span></div>
  })}</div>
}

export function SectionList({ sections, fallback }: { sections: TimelineSection[]; fallback: string }) {
  if (sections.length === 0) return <MarkdownBody text={fallback} />
  return (
    <div className="timeline-sections">
      {sections.map((section, index) => (
        <section className={`timeline-section section-${section.kind}`} key={`${section.label}:${index}`}>
          <h4>{section.label}</h4>
          {section.kind === 'diff'
            ? <DiffBody text={section.content} />
            : section.kind === 'text'
              ? <MarkdownBody text={section.content} />
              : <pre><code>{section.content || '（空）'}</code></pre>}
        </section>
      ))}
    </div>
  )
}

export function MarkdownBody({ text }: { text: string }) {
  const selectionGuard = useSelectionGuard()
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[rehypeKatex]}
      components={{
        pre: ({ children }) => <CodeBlock>{children}</CodeBlock>,
        table: ({ children }) => <div className="markdown-table-scroll"><table>{children}</table></div>,
        a: ({ href, children }) => (
          <a
            href={href}
            target="_blank"
            rel="noreferrer"
            {...selectionGuard(() => {
              if (href !== undefined) window.open(href, '_blank', 'noopener,noreferrer')
            }, true)}
          >
            {children}
          </a>
        ),
        code: ({ className, children }) => {
          const language = /language-([\w-]+)/.exec(className ?? '')?.[1]
          const code = String(children).replace(/\n$/, '')
          return language === undefined ? <code>{children}</code> : <HighlightedCode code={code} language={language} />
        },
      }}
    >
      {text || ' '}
    </ReactMarkdown>
  )
}

function CodeBlock({ children }: { children: ReactNode }) {
  const props = isValidElement<{ children?: ReactNode; className?: string }>(children) ? children.props : undefined
  const text = String(props?.children ?? '').replace(/\n$/, '')
  const language = /language-([\w-]+)/.exec(props?.className ?? '')?.[1] ?? ''
  return <div className="code-block"><div className="code-block-header"><span>{language || '代码'}</span><CopyButton text={text} label="复制代码" /></div>{language ? <HighlightedCode code={text} language={language} /> : <pre><code>{text}</code></pre>}</div>
}

function HighlightedCode({ code, language }: { code: string; language: string }) {
  const [tokens, setTokens] = useState<Awaited<ReturnType<typeof highlightCode>> | null>(null)
  useEffect(() => {
    let current = true
    void highlightCode(code, language).then((result) => { if (current) setTokens(result) })
    return () => { current = false }
  }, [code, language])
  if (tokens === null) return <pre><code>{code}</code></pre>
  return <pre className="highlighted-code"><code>{tokens.map((line, row) => <span key={row}>{line.map((token, column) => <span key={column} className="code-token" style={{'--code-light': token.color, '--code-dark': token.darkColor, fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined, fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined} as CSSProperties}>{token.content}</span>)}{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
}

function DiffBody({ text }: { text: string }) {
  let patches: ReturnType<typeof parsePatch> = []
  try { patches = parsePatch(text) } catch { /* Non-unified output remains readable. */ }
  return <div className="file-output"><OutputHeader text={text} label="文件改动" />
    {patches.length === 0 ? <pre>{text}</pre> : patches.map((patch, patchIndex) => <div className="tool-lines" key={patchIndex}>
      <div className="file-output-name">{patch.newFileName}</div>
      {patch.hunks.map((hunk, hunkIndex) => {
        let oldLine = hunk.oldStart, newLine = hunk.newStart
        return <div key={hunkIndex}><div className="diff-hunk">@@ −{hunk.oldStart},{hunk.oldLines} +{hunk.newStart},{hunk.newLines} @@</div>
          {hunk.lines.map((line, index) => {
            const marker = line[0]
            const before = marker === '-' || marker === ' ' ? oldLine++ : ''
            const after = marker === '+' || marker === ' ' ? newLine++ : ''
            return <div key={index} className={`tool-line ${marker === '+' ? 'diff-add' : marker === '-' ? 'diff-remove' : 'diff-context'}`}><span className="tool-line-number">{before}</span><span className="tool-line-number">{after}</span><span>{line}</span></div>
          })}
        </div>
      })}
    </div>)}
  </div>
}

function preview(text: string): string {
  return text.split('\n').slice(0, previewLineCount).join('\n')
}

function statusLabel(status: TimelineItemModel['status']): string {
  return ({ stable: '', running: '进行中', completed: '已完成', failed: '失败', interrupted: '已停止', pending: '等待中' } as const)[status]
}

const stepKinds = new Set<TimelineItemModel['kind']>(['thinking', 'tool', 'diff', 'diagnostic', 'control', 'unknown'])

function oneLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}



function StepIcon({ item }: Props) {
  const path = item.title === 'bash' ? 'm4 6 5 6-5 6m8 0h8'
    : item.title === 'grep' || item.title === 'glob' ? 'M15 15l6 6M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0'
      : item.kind === 'diff' ? 'm14 3 7 7M4 20l4-1L20 7l-3-3L5 16z'
        : 'M14 2H5v20h14V7zM14 2v6h5M8 12h8M8 16h8'
  return <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>
}
