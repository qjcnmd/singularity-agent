//! 正文渲染：Markdown（GFM、数学公式）与代码块布局。
//!
//! 模型正文的 Markdown 渲染和代码块布局，各处共用。时间线条目的状态不在这里，
//! 代码高亮在 highlight。

import { isValidElement, memo, useLayoutEffect, useRef, useState, type ReactNode } from 'react'
import { useReducedMotion } from 'motion/react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import 'katex/dist/katex.min.css'
import { CopyButton } from './components/CopyButton'
import { CodeTokens, useCodeTokens } from './highlight'
import { useSelectionGuard } from './interactions'

export function MarkdownBody({ text, streaming = false }: { text: string; streaming?: boolean }) {
  return <RenderedMarkdown text={useStreamedText(text, streaming)} />
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
  return <a href={href} target="_blank" rel="noreferrer" {...selectionGuard()}>{children}</a>
}

const markdownComponents: Components = { pre: CodeBlock, table: MarkdownTable, a: MarkdownLink }

function CodeBlock({ children }: { children?: ReactNode }) {
  const props = isValidElement<{ children?: ReactNode; className?: string }>(children) ? children.props : undefined
  const text = String(props?.children ?? '').replace(/\n$/, '')
  const language = /language-([\w-]+)/.exec(props?.className ?? '')?.[1] ?? ''
  return <div className="code-block"><div className="code-block-header"><span>{language || '代码'}</span><CopyButton text={text} label="复制代码" /></div>{language ? <HighlightedCode code={text} language={language} /> : <pre><code>{text}</code></pre>}</div>
}

/** 代码块布局；高亮 token 由公共 highlight 模块提供，未就绪时落回原文。 */
const HighlightedCode = memo(function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre className="highlighted-code"><code>{tokens === null ? code : tokens.map((line, row) => <span key={row}><CodeTokens tokens={line} />{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
})
