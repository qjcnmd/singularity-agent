/**
 * 正文渲染：Markdown（GFM、数学公式）、流式文本平滑揭示与代码块高亮。
 */

import { createContext, useContext, Children, isValidElement, memo, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useReducedMotion } from 'motion/react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import 'katex/dist/katex.min.css'
import { CopyButton } from './components/CopyButton'
import { MermaidDiagram } from './components/MermaidDiagram'
import { CodeTokens, useCodeTokens } from './highlight'
import { useSelectionGuard } from './interactions'
import { appStore } from './appStore'

export function MarkdownBody({ text, streaming = false }: { text: string; streaming?: boolean }) {
  return <Streaming.Provider value={streaming}><RenderedMarkdown text={useStreamedText(text, streaming)} /></Streaming.Provider>
}

/** 只平滑正在输出的正文；历史、替换与终态直接显示完整文本。 */
function useStreamedText(text: string, streaming: boolean) {
  const reducedMotion = useReducedMotion()
  const [visible, setVisible] = useState(text)
  const shown = useRef(text)
  const lastPaint = useRef(0)
  useLayoutEffect(() => {
    if (!streaming || reducedMotion || !text.startsWith(shown.current)) {
      shown.current = text
      setVisible(text)
      return
    }
    const start = shown.current.length
    if (start === text.length) return
    const started = performance.now()
    let frame: number
    const reveal = (now: number) => {
      // 把服务端成批到达的文字摊到 80ms 里送完，不一截一截跳出。
      const progress = Math.min(1, (now - started) / 80)
      // 高刷新率屏幕也不必每帧重新解析 Markdown。
      if (progress === 1 || now - lastPaint.current >= 1000 / 60) {
        let end = start + Math.ceil((text.length - start) * progress)
        // 不在 UTF-16 代理对中间截断 emoji 等字符。
        const previous = text.charCodeAt(end - 1)
        if (end < text.length && previous >= 0xd800 && previous <= 0xdbff) end += 1
        shown.current = text.slice(0, end)
        lastPaint.current = now
        setVisible(shown.current)
      }
      if (progress < 1) frame = requestAnimationFrame(reveal)
    }
    frame = requestAnimationFrame(reveal)
    return () => cancelAnimationFrame(frame)
  }, [text, streaming, reducedMotion])
  return !streaming || reducedMotion || !text.startsWith(visible) ? text : visible
}

const RenderedMarkdown = memo(function RenderedMarkdown({ text }: { text: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[rehypeKatex]}
      components={markdownComponents}
    >
      {text || ' '}
    </ReactMarkdown>
  )
})

function MarkdownTable({ children }: { children?: ReactNode }) {
  return <div className="markdown-table-scroll"><table>{children}</table></div>
}

function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }) {
  const selectionGuard = useSelectionGuard()
  // 打开链接仍由 Chromium 的原生行为处理（包括键盘和修饰键点击）；这里只拦下拖选文字后的误点。
  return <a href={href} target="_blank" rel="noreferrer" {...selectionGuard()}><StreamText>{children}</StreamText></a>
}

/** 行内代码里的相对路径按这个目录解析；为空时用当前任务的工作目录。文件栏里的 Markdown 用文件自己所在的目录。 */
export const PathBase = createContext<string | null>(null)

// 带目录分隔符的“xx/yy.ext”，或常见扩展名的单个文件名；可带 :行号:列号。`state.session` 这种属性访问不算。
const pathLike = /^(?:~[\\/]|[A-Za-z]:[\\/]|\.{0,2}[\\/])?(?:[\w.@+-]+[\\/])+[\w.@+-]+\.[A-Za-z0-9]{1,10}(?::\d+){0,2}$/
const fileNameLike = /^[\w.@+-]+\.(?:md|markdown|txt|json|jsonc|toml|ya?ml|ts|tsx|js|jsx|mjs|cjs|rs|py|go|java|c|h|cpp|css|html|sh|ps1|lock|env)(?::\d+){0,2}$/i

function InlineCode({ children, className }: { children?: ReactNode; className?: string }) {
  const base = useContext(PathBase)
  const streaming = useContext(Streaming)
  const selectionGuard = useSelectionGuard()
  const text = typeof children === 'string' ? children.trim() : ''
  if (streaming || className || !(pathLike.test(text) || fileNameLike.test(text))) return <code className={className}><StreamText>{children}</StreamText></code>
  const path = text.replace(/(?::\d+){1,2}$/, '')
  const open = () => {
    const cwd = base ?? appStore.getSnapshot().session?.summary.cwd
    if (cwd) appStore.openFile(path, cwd)
  }
  return <code className="file-link" role="link" tabIndex={0} title={`在文件栏打开 ${path}`} {...selectionGuard(open)}
    onKeyDown={event => { if (event.key === 'Enter') open() }}>{children}</code>
}

const Streaming = createContext(false)
function StreamText({ children }: { children?: ReactNode }) {
  const streaming = useContext(Streaming)
  const reduced = useReducedMotion()
  if (!streaming || reduced) return children
  return Children.map(children, child => typeof child === 'string'
    ? Array.from(child).map((character, index) => <span className="stream-char" key={index}>{character}</span>)
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
  // 流式输出时代码还不完整，先显示代码，回复完成后再画图。
  return language === 'mermaid' && !streaming ? <MermaidDiagram code={text} fallback={block} /> : block
}

const HighlightedCode = memo(function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre className="highlighted-code"><code>{tokens === null ? <StreamText>{code}</StreamText> : tokens.map((line, row) => <span key={row}><CodeTokens tokens={line} renderText={text => <StreamText>{text}</StreamText>} />{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
})
