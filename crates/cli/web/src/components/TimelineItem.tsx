import { useEffect, useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import { formatElapsed, uiText } from '../copy'
import { highlightCode } from '../highlight'
import { useSelectionGuard } from '../interactions'
import type { TimelineItemModel, TimelineSection } from '../timeline'

interface Props {
  item: TimelineItemModel
  selected: boolean
  onSelect: (item: TimelineItemModel) => void
}

export function TimelineItem({ item, selected, onSelect }: Props) {
  const isStep = stepKinds.has(item.kind)
  const canCollapse = isStep || item.hiddenLines > 0
  const [expanded, setExpanded] = useState(!isStep)
  const [copied, setCopied] = useState(false)
  const selectionGuard = useSelectionGuard()

  const copy = async () => {
    await navigator.clipboard.writeText(item.detail)
    setCopied(true)
    window.setTimeout(() => setCopied(false), 1_200)
  }

  if (item.kind === 'terminal') {
    return (
      <div className={`turn-terminal status-${item.status}`} data-item-id={item.key} role="status">
        <span className="terminal-mark" aria-hidden="true" />
        <strong>{item.title}</strong>
        {item.body !== '' && <span>{item.body}</span>}
      </div>
    )
  }

  if (item.kind === 'user' || item.kind === 'assistant') {
    const body = canCollapse && !expanded ? preview(item.body) : item.body
    return (
      <article
        className={`timeline-item message-item timeline-${item.kind} status-${item.status}${selected ? ' is-selected' : ''}`}
        data-item-id={item.key}
        aria-label={`${item.title}，${statusLabel(item.status) || '已记录'}`}
      >
        <header className="message-header">
          <span className="message-avatar" aria-hidden="true">{item.kind === 'assistant' ? 'S' : '你'}</span>
          <strong>{item.title}</strong>
          {item.status === 'running' && <span className="live-label">{item.kind === 'user' ? '已发送' : '正在回复'}</span>}
          <button type="button" className="copy-button" {...selectionGuard(() => { void copy() })}>{copied ? uiText.copied : uiText.copy}</button>
        </header>
        <div className="timeline-body message-body"><MarkdownBody text={body} /></div>
        {canCollapse && (
          <button type="button" className="expand-button" {...selectionGuard(() => setExpanded((value) => !value))}>
            {expanded ? '收起' : `展开全文 · 还有 ${item.hiddenLines} 行`}
          </button>
        )}
      </article>
    )
  }

  return (
    <article
      className={`timeline-item activity-step timeline-${item.kind} status-${item.status}${selected ? ' is-selected' : ''}`}
      data-item-id={item.key}
      aria-label={`${item.title}，${statusLabel(item.status) || '已记录'}`}
    >
      <header className="activity-step-header">
        <button type="button" className="activity-toggle" {...selectionGuard(() => setExpanded((value) => !value))} aria-expanded={expanded}>
          <span className="step-chevron" aria-hidden="true">{expanded ? '⌄' : '›'}</span>
          <span className="step-icon" aria-hidden="true">{kindMark(item.kind)}</span>
          <span className="step-copy">
            <strong>{item.title}</strong>
            {item.body !== '' && <small>{oneLine(item.body)}</small>}
            {item.startedAt !== null && item.status === 'running' && <small>{formatElapsed(item.startedAt)}</small>}
          </span>
          <span className="step-meta">
            {item.addedLines > 0 && <span className="diff-stat is-added">+{item.addedLines}</span>}
            {item.removedLines > 0 && <span className="diff-stat is-removed">−{item.removedLines}</span>}
            {statusLabel(item.status) !== '' && <span className="item-status">{statusLabel(item.status)}</span>}
          </span>
        </button>
        <span className="item-actions">
          <button type="button" className="quiet-button" {...selectionGuard(() => { void copy() })}>{copied ? uiText.copied : uiText.copy}</button>
          <button type="button" className="quiet-button" {...selectionGuard(() => onSelect(item))}>{uiText.details}</button>
        </span>
      </header>
      {expanded && (
        <div className="timeline-body activity-output">
          <SectionList sections={item.sections} fallback={item.detail} />
          {item.hiddenLines > 0 && <span className="line-count">共 {item.detail.split('\n').length} 行</span>}
        </div>
      )}
    </article>
  )
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

function MarkdownBody({ text }: { text: string }) {
  const selectionGuard = useSelectionGuard()
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm]}
      components={{
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

function HighlightedCode({ code, language }: { code: string; language: string }) {
  const [html, setHtml] = useState<string | null>(null)
  useEffect(() => {
    let current = true
    void highlightCode(code, language).then((result) => { if (current) setHtml(result) })
    return () => { current = false }
  }, [code, language])
  if (html === null) return <pre><code>{code}</code></pre>
  return <div className="highlighted-code" dangerouslySetInnerHTML={{ __html: html }} />
}

function DiffBody({ text }: { text: string }) {
  return (
    <pre className="diff-body">
      {text.split('\n').map((line, index) => (
        <span
          key={`${index}:${line.slice(0, 16)}`}
          className={line.startsWith('+') && !line.startsWith('+++')
            ? 'diff-add'
            : line.startsWith('-') && !line.startsWith('---')
              ? 'diff-remove'
              : 'diff-context'}
        >
          {line || ' '}{'\n'}
        </span>
      ))}
    </pre>
  )
}

function preview(text: string): string {
  return text.split('\n').slice(0, 8).join('\n')
}

function statusLabel(status: TimelineItemModel['status']): string {
  return ({ stable: '', running: '进行中', completed: '已完成', failed: '失败', interrupted: '已停止', pending: '等待中' } as const)[status]
}

const stepKinds = new Set<TimelineItemModel['kind']>(['thinking', 'tool', 'diff', 'diagnostic', 'control', 'unknown'])

function oneLine(text: string): string {
  return text.replace(/\s+/g, ' ').trim()
}

function kindMark(kind: TimelineItemModel['kind']): string {
  const marks: Partial<Record<TimelineItemModel['kind'], string>> = {
    thinking: '∿',
    tool: '⌘',
    diff: '±',
    diagnostic: '·',
    control: '↳',
    unknown: '?',
  }
  return marks[kind] ?? '·'
}
