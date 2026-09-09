import { CopyButton } from './CopyButton'
import { ExpandChevron } from './ExpandChevron'
import { Disclosure } from './Disclosure'
import { useEffect, useLayoutEffect, useRef, useState, isValidElement, type CSSProperties, type ReactNode } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import Anser from 'anser'
import type { StructuredPatch } from 'diff'
import { Pencil } from 'lucide-react'
import { diffContext } from '../diffView'
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
          <button type="button" className="expand-button" aria-expanded={expanded} {...selectionGuard(() => setExpanded((value) => !value))}>
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
        <StepLabel item={item} icon={<StepIcon item={item} />} />
        <ExpandChevron expanded={expanded} className="step-chevron" />
        <span className="step-separator" aria-hidden="true">·</span>
        <span className="step-summary">{failure ?? oneLine(item.body)}</span>
        {item.addedLines > 0 && <span className="diff-stat is-added">+{item.addedLines}</span>}
        {item.removedLines > 0 && <span className="diff-stat is-removed">−{item.removedLines}</span>}
        {['failed', 'interrupted'].includes(item.status) && <span className="item-status">{statusLabel(item.status)}</span>}
      </button>
      <Disclosure open={expanded}><div className="activity-expanded">
        <div className="timeline-body activity-output"><ToolOutput item={item} /></div>
      </div></Disclosure>
    </article>
  )
}

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
  const running = item.status === 'running'
  const text = item.body.trim().replace(/\n[\t \r]*\n+/g, '\n')
  const summary = running ? text.slice(text.lastIndexOf('\n') + 1) : text.split('\n')[0]
  useLayoutEffect(() => {
    const node = summaryRef.current, measure = measureRef.current
    if (!node || !measure) return
    const update = () => {
      if (showFullText) return
      const overflow = text.includes('\n') || measure.getBoundingClientRect().width > node.clientWidth + 1
      setCanExpand(overflow)
      if (!overflow) setExpanded(false)
    }
    update()
    const observer = new ResizeObserver(update)
    observer.observe(node)
    observer.observe(measure)
    return () => observer.disconnect()
  }, [text, summary, canExpand, showFullText])
  useEffect(() => {
    if (summaryRef.current !== null) summaryRef.current.scrollLeft = running && !expanded ? summaryRef.current.scrollWidth : 0
  }, [summary, running, expanded])
  const Row = canExpand ? motion.button : motion.div
  return <article className={`timeline-item reasoning-row status-${item.status}${showFullText ? ' is-expanded' : ''}`} data-item-id={item.key}>
    <Row initial={false} animate={{ height: expanded ? 'auto' : 24 }} transition={{ duration: reducedMotion ? 0 : 0.28, ease: [0.2, 0.8, 0.2, 1] }} onAnimationComplete={() => { if (!expanded) setClosing(false) }} type={canExpand ? 'button' : undefined} className="activity-toggle" aria-expanded={canExpand ? expanded : undefined} {...(canExpand ? guard(() => { setClosing(expanded && !reducedMotion); setExpanded(value => !value) }) : {})}>
      <StepLabel item={item} />
      {canExpand ? <ExpandChevron expanded={expanded} className={`step-chevron${expanded ? ' is-open' : ''}`} /> : <span className="step-chevron" aria-hidden="true" />}
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
  if (!item.tool) return <SectionList sections={item.sections} fallback={item.body} />
  const { args: input, output, diff, patches } = item.tool
  if (diff !== '') return <DiffBody text={diff} patches={patches} />
  const args = typeof input === 'object' && input !== null ? input as Record<string, unknown> : {}
  const command = typeof args.command === 'string' ? args.command : typeof args.cmd === 'string' ? args.cmd : null
  if (command !== null) return <div className="terminal-output">
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <><OutputHeader label="输出" /><pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{ color: part.fg ? `rgb(${part.fg})` : undefined, backgroundColor: part.bg ? `rgb(${part.bg})` : undefined, fontWeight: part.decorations.includes('bold') ? 700 : undefined }}>{part.content}</span>)}</pre></>}
  </div>
  if (item.filePath !== null && output !== '' && item.title === 'read' && item.status !== 'failed') return <div className="file-output">
    <OutputHeader label={item.filePath} /><NumberedOutput text={output} startLine={typeof args.offset === 'number' ? args.offset : 1} />
  </div>
  if (output !== '' && (item.title.toLowerCase() === 'grep' || item.title.toLowerCase() === 'glob')) return <div className="file-output"><OutputHeader label="搜索结果" /><NumberedOutput text={output} /></div>
  const sections: TimelineSection[] = [{ label: '参数', content: JSON.stringify(input, null, 2), kind: 'json' }]
  if (output !== '') sections.push({ label: item.status === 'failed' ? '错误' : '输出', content: output, kind: item.status === 'failed' ? 'error' : 'code' })
  return <SectionList sections={sections} fallback={item.body} />
}

function OutputHeader({ label }: { label: string }) {
  return <div className="tool-output-header">{label}</div>
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
          {section.kind === 'text'
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

type CodeLine = Awaited<ReturnType<typeof highlightCode>>[number]

function useCodeTokens(code: string, language: string) {
  const [result, setResult] = useState<{ code: string; language: string; tokens: CodeLine[] } | null>(null)
  useEffect(() => {
    let current = true
    void highlightCode(code, language).then(tokens => { if (current) setResult({ code, language, tokens }) })
    return () => { current = false }
  }, [code, language])
  return result?.code === code && result.language === language ? result.tokens : null
}

function CodeTokens({ tokens, fallback }: { tokens?: CodeLine; fallback: string }) {
  return tokens === undefined ? fallback : tokens.map((token, column) => <span key={column} className="code-token" style={{ '--code-light': token.color, '--code-dark': token.darkColor, fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined, fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined } as CSSProperties}>{token.content}</span>)
}

function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre className="highlighted-code"><code>{tokens === null ? code : tokens.map((line, row) => <span key={row}><CodeTokens tokens={line} fallback="" />{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
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

function statusLabel(status: TimelineItemModel['status']): string {
  return ({ stable: '', running: '进行中', completed: '已完成', failed: '失败', interrupted: '已停止', pending: '等待中' } as const)[status]
}

const stepKinds = new Set<TimelineItemModel['kind']>(['thinking', 'tool', 'diff', 'diagnostic', 'control', 'unknown'])

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
