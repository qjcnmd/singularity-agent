//! 正文与代码渲染：Markdown（GFM、数学公式）与带高亮的代码块。
//!
//! 时间线条目与轨迹详情都显示模型正文，两者的渲染规则因此必须同源；这里只
//! 承载这些共用渲染，不持有时线条目的编排状态。diff 行也从同一份 token 渲染
//! 取用高亮结果，不另建第二套高亮。

import { useEffect, useState, isValidElement, type CSSProperties, type ReactNode } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import 'katex/dist/katex.min.css'
import { CopyButton } from './components/CopyButton'
import { highlightCode } from './highlight'
import { useSelectionGuard } from './interactions'

export function MarkdownBody({ text }: { text: string }) {
  return (
    <ReactMarkdown
      remarkPlugins={[remarkGfm, remarkMath]}
      rehypePlugins={[rehypeKatex]}
      components={markdownComponents}
    >
      {text || ' '}
    </ReactMarkdown>
  )
}

function MarkdownTable({ children }: { children?: ReactNode }) {
  return <div className="markdown-table-scroll"><table>{children}</table></div>
}

function MarkdownLink({ href, children }: { href?: string; children?: ReactNode }) {
  const selectionGuard = useSelectionGuard()
  // 导航仍由浏览器按 a 的原生语义处理（含键盘与修饰键点击）；这里只拦下拖选后的误触。
  return <a href={href} target="_blank" rel="noreferrer" {...selectionGuard()}>{children}</a>
}

const markdownComponents: Components = { pre: CodeBlock, table: MarkdownTable, a: MarkdownLink }

function CodeBlock({ children }: { children?: ReactNode }) {
  const props = isValidElement<{ children?: ReactNode; className?: string }>(children) ? children.props : undefined
  const text = String(props?.children ?? '').replace(/\n$/, '')
  const language = /language-([\w-]+)/.exec(props?.className ?? '')?.[1] ?? ''
  return <div className="code-block"><div className="code-block-header"><span>{language || '代码'}</span><CopyButton text={text} label="复制代码" /></div>{language ? <HighlightedCode code={text} language={language} /> : <pre><code>{text}</code></pre>}</div>
}

type CodeLine = Awaited<ReturnType<typeof highlightCode>>[number]

/** 一段代码的高亮行；`null` 表示尚未就绪，调用方落回原文。同一段代码只取一次。 */
export function useCodeTokens(code: string, language: string) {
  const [result, setResult] = useState<{ code: string; language: string; tokens: CodeLine[] } | null>(null)
  useEffect(() => {
    let current = true
    void highlightCode(code, language).then(
      tokens => { if (current) setResult({ code, language, tokens }) },
      // 语言分块或高亮失败：保持 null，落回既有原文展示，不留下未处理的拒绝。
      () => {},
    )
    return () => { current = false }
  }, [code, language])
  return result?.code === code && result.language === language ? result.tokens : null
}

/** 单行高亮 token；`undefined` 表示该行还没有高亮结果，调用方落回原文。 */
export function CodeTokens({ tokens, fallback }: { tokens?: CodeLine; fallback: string }) {
  return tokens === undefined ? fallback : tokens.map((token, column) => <span key={column} className="code-token" style={{ ...token.htmlStyle, fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined, fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined } as CSSProperties}>{token.content}</span>)
}

function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre className="highlighted-code"><code>{tokens === null ? code : tokens.map((line, row) => <span key={row}><CodeTokens tokens={line} fallback="" />{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
}
