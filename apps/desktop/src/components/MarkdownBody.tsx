import { createContext, useContext, Children, isValidElement, memo, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useReducedMotion } from 'motion/react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import 'katex/dist/katex.min.css'
import { appStore } from '../appStore'
import { CodeTokens, useCodeTokens } from '../highlight'
import { useSelectionGuard } from '../interactions'
import { CopyButton } from './CopyButton'
import { MermaidDiagram } from './MermaidDiagram'

export function MarkdownBody({ text, streaming = false }: { text: string; streaming?: boolean }) {
  return <Streaming.Provider value={streaming}><RenderedMarkdown text={useStreamedText(text, streaming)} /></Streaming.Provider>
}

function useStreamedText(text: string, streaming: boolean) {
  const reducedMotion = useReducedMotion()
  const [visible, setVisible] = useState(text)
  const targetRef = useRef(text)
  const shownRef = useRef(text)
  const frameRef = useRef(0)
  const lastPaintRef = useRef(0)
  targetRef.current = text

  useLayoutEffect(() => {
    const cancel = () => {
      if (frameRef.current) {
        cancelAnimationFrame(frameRef.current)
        frameRef.current = 0
      }
    }
    if (!streaming || reducedMotion || !text.startsWith(shownRef.current)) {
      cancel()
      shownRef.current = text
      setVisible(text)
      return
    }
    if (shownRef.current.length >= text.length || frameRef.current) return
    lastPaintRef.current = performance.now()
    const reveal = (now: number) => {
      const target = targetRef.current
      if (!target.startsWith(shownRef.current)) {
        frameRef.current = 0
        shownRef.current = target
        setVisible(target)
        return
      }
      const remaining = target.length - shownRef.current.length
      if (remaining <= 0) {
        frameRef.current = 0
        return
      }
      const dt = now - lastPaintRef.current
      const minInterval = Math.min(64, 14 + Math.floor(target.length / 400))
      if (dt >= minInterval || remaining <= 2) {
        const step = remaining <= 2 ? remaining : Math.max(1, Math.ceil(remaining * (1 - Math.exp(-Math.min(dt, 96) / 45))))
        let end = Math.min(target.length, shownRef.current.length + step)
        const previous = target.charCodeAt(end - 1)
        if (end < target.length && previous >= 0xd800 && previous <= 0xdbff) end += 1
        shownRef.current = target.slice(0, end)
        lastPaintRef.current = now
        setVisible(shownRef.current)
      }
      if (shownRef.current.length < target.length) frameRef.current = requestAnimationFrame(reveal)
      else frameRef.current = 0
    }
    frameRef.current = requestAnimationFrame(reveal)
  }, [text, streaming, reducedMotion])

  useEffect(() => () => {
    if (frameRef.current) {
      cancelAnimationFrame(frameRef.current)
      frameRef.current = 0
    }
  }, [])

  return !streaming || reducedMotion || !text.startsWith(visible) ? text : visible
}

type KatexRenderer = typeof import('rehype-katex')['default']
let katexRenderer: KatexRenderer | undefined
let loadingKatex: Promise<KatexRenderer | undefined> | undefined
const RequestKatex = createContext<(needed: true) => void>(() => {})

const RenderedMarkdown = memo(function RenderedMarkdown({ text }: { text: string }) {
  const [rehypeKatex, setRehypeKatex] = useState(() => katexRenderer)
  const [needsKatex, requestKatex] = useState(false)
  useEffect(() => {
    if (!needsKatex || rehypeKatex) return
    let active = true
    loadingKatex ??= import('rehype-katex').then(({ default: renderer }) => katexRenderer = renderer, () => {
      loadingKatex = undefined
      return undefined
    })
    void loadingKatex.then(renderer => { if (active && renderer) setRehypeKatex(() => renderer) })
    return () => { active = false }
  }, [needsKatex, rehypeKatex])
  return (
    <RequestKatex.Provider value={requestKatex}>
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkMath]}
        rehypePlugins={rehypeKatex ? [rehypeKatex] : undefined}
        components={markdownComponents}
      >
        {text || ' '}
      </ReactMarkdown>
    </RequestKatex.Provider>
  )
})

function MathSource({ children }: { children: ReactNode }) {
  const requestKatex = useContext(RequestKatex)
  useEffect(() => { requestKatex(true) }, [requestKatex])
  return children
}

function MarkdownTable({ children }: { children?: ReactNode }) {
  return <div className="markdown-table-scroll"><table>{children}</table></div>
}

function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }) {
  const selectionGuard = useSelectionGuard()
  return <a href={href} target="_blank" rel="noreferrer" {...selectionGuard()}><StreamText>{children}</StreamText></a>
}

export const PathBase = createContext<string | null>(null)

const pathLike = /^(?:(?:~|[A-Za-z]:|\.{1,2})[\\/]|[\\/]|[\p{L}\p{M}\p{N}_.@+() -]+[\\/])(?:[\p{L}\p{M}\p{N}_.@+() -]+[\\/])*(?:\.[\p{L}\p{M}\p{N}_.@+() -]+|[\p{L}\p{M}\p{N}_.@+() -]+\.[A-Za-z0-9]{1,10})(?::\d+){0,2}$/u
const fileNameLike = /^(?:\.[\p{L}\p{M}\p{N}_.@+() -]+|[\p{L}\p{M}\p{N}_.@+() -]+\.(?:md|markdown|txt|json|jsonc|toml|ya?ml|ts|tsx|js|jsx|mjs|cjs|rs|py|go|java|c|h|cpp|css|html|sh|ps1|lock|env))(?::\d+){0,2}$/iu

function InlineCode({ children, className }: { children?: ReactNode; className?: string }) {
  const base = useContext(PathBase)
  const streaming = useContext(Streaming)
  const selectionGuard = useSelectionGuard()
  const text = typeof children === 'string' ? children.trim() : ''
  if (streaming || className || !(pathLike.test(text) || fileNameLike.test(text))) {
    const code = <code className={className}><StreamText>{children}</StreamText></code>
    return className?.split(' ').includes('language-math') ? <MathSource>{code}</MathSource> : code
  }
  const path = text.replace(/(?::\d+){1,2}$/, '')
  const open = () => {
    const cwd = base ?? appStore.getSnapshot().session?.summary.cwd
    if (cwd) appStore.openFile(path, cwd)
  }
  return <code className="file-link" role="link" tabIndex={0} title={`在文件栏打开 ${path}`} {...selectionGuard(open)}
    onKeyDown={event => { if (event.key === 'Enter') open() }}>{children}</code>
}

const Streaming = createContext(false)
const streamFadeMs = 180
const streamChunkStyle = { animationDuration: `${streamFadeMs}ms` }

type StreamChunk = { id: number; text: string; expiresAt: number }
type StreamChunkState = { settled: string; chunks: StreamChunk[]; seen: string; nextId: number }

function StreamingText({ text }: { text: string }) {
  const [state, setState] = useState<StreamChunkState>(() => ({
    settled: '',
    chunks: text ? [{ id: 0, text, expiresAt: performance.now() + streamFadeMs }] : [],
    seen: text,
    nextId: 1,
  }))
  useLayoutEffect(() => {
    setState(prev => {
      if (text === prev.seen) return prev
      const expiresAt = performance.now() + streamFadeMs
      if (text.startsWith(prev.seen)) {
        const delta = text.slice(prev.seen.length)
        return {
          settled: prev.settled,
          chunks: [...prev.chunks, { id: prev.nextId, text: delta, expiresAt }],
          seen: text,
          nextId: prev.nextId + 1,
        }
      }
      return {
        settled: '',
        chunks: text ? [{ id: prev.nextId, text, expiresAt }] : [],
        seen: text,
        nextId: prev.nextId + 1,
      }
    })
  }, [text])
  const nextExpiry = state.chunks[0]?.expiresAt
  useEffect(() => {
    if (nextExpiry === undefined) return
    const delay = Math.max(0, nextExpiry - performance.now())
    const timer = setTimeout(() => {
      const now = performance.now() + 8
      setState(prev => {
        if (prev.chunks.length === 0) return prev
        let split = 0
        while (split < prev.chunks.length && prev.chunks[split].expiresAt <= now) split += 1
        if (split === 0) return prev
        let settled = prev.settled
        for (let i = 0; i < split; i += 1) settled += prev.chunks[i].text
        return { ...prev, settled, chunks: prev.chunks.slice(split) }
      })
    }, delay)
    return () => clearTimeout(timer)
  }, [nextExpiry])
  return <>{state.settled}{state.chunks.map(chunk =>
    <span className="stream-char" style={streamChunkStyle} key={chunk.id}>{chunk.text}</span>)}</>
}

function StreamText({ children }: { children?: ReactNode }) {
  const streaming = useContext(Streaming)
  const reduced = useReducedMotion()
  if (!streaming || reduced) return children
  return Children.map(children, child => typeof child === 'string'
    ? <StreamingText text={child} />
    : child)
}

const markdownComponents: Components = {
  pre: CodeBlock, table: MarkdownTable, a: MarkdownLink,
  code: InlineCode,
  h4: ({ children }) => <h4><StreamText>{children}</StreamText></h4>,
  h5: ({ children }) => <h5><StreamText>{children}</StreamText></h5>,
  h6: ({ children }) => <h6><StreamText>{children}</StreamText></h6>,
  td: ({ children }) => <td><StreamText>{children}</StreamText></td>,
  th: ({ children }) => <th><StreamText>{children}</StreamText></th>,
  del: ({ children }) => <del><StreamText>{children}</StreamText></del>,
  p: ({ children }) => <p><StreamText>{children}</StreamText></p>,
  li: ({ children }) => <li><StreamText>{children}</StreamText></li>,
  strong: ({ children }) => <strong><StreamText>{children}</StreamText></strong>,
  em: ({ children }) => <em><StreamText>{children}</StreamText></em>,
  h1: ({ children }) => <h1><StreamText>{children}</StreamText></h1>,
  h2: ({ children }) => <h2><StreamText>{children}</StreamText></h2>,
  h3: ({ children }) => <h3><StreamText>{children}</StreamText></h3>,
}

function CodeBlock({ children }: { children?: ReactNode }) {
  const props = isValidElement<{ children?: ReactNode; className?: string }>(children) ? children.props : undefined
  const text = String(props?.children ?? '').replace(/\n$/, '')
  const language = /language-([\w-]+)/.exec(props?.className ?? '')?.[1] ?? ''
  const streaming = useContext(Streaming)
  const body = language ? <HighlightedCode code={text} language={language} /> : <pre><code><StreamText>{text}</StreamText></code></pre>
  const block = <div className="code-block"><div className="code-block-header"><span>{language || '代码'}</span><CopyButton text={text} label="复制代码" /></div>{body}</div>
  if (props?.className?.split(' ').includes('language-math')) return <MathSource>{block}</MathSource>
  return language === 'mermaid' && !streaming ? <MermaidDiagram code={text} fallback={block} /> : block
}

const HighlightedCode = memo(function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre><code>{tokens === null ? code : tokens.map((line, row) => <HighlightedLine key={row} tokens={line} newline={row < tokens.length - 1} />)}</code></pre>
})

const HighlightedLine = memo(function HighlightedLine({ tokens, newline }: { tokens: Parameters<typeof CodeTokens>[0]['tokens']; newline: boolean }) {
  return <span><CodeTokens tokens={tokens} />{newline ? '\n' : ''}</span>
})
