//! 正文渲染：Markdown（GFM、数学公式）与代码块布局。
//!
//! 时间线条目与轨迹详情都显示模型正文，两者的渲染规则因此必须同源；这里只
//! 承载这些共用渲染与代码块布局，不持有时线条目的编排状态，也不承载代码高亮
//! 本身——高亮由 highlight 模块提供给 Markdown 与 Diff 两个消费者。

import { isValidElement, memo, type ReactNode } from 'react'
import ReactMarkdown, { type Components } from 'react-markdown'
import remarkGfm from 'remark-gfm'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import 'katex/dist/katex.min.css'
import { CopyButton } from './components/CopyButton'
import { CodeTokens, useCodeTokens } from './highlight'
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

/** 代码块布局；高亮 token 由公共 highlight 模块提供，未就绪时落回原文。 */
const HighlightedCode = memo(function HighlightedCode({ code, language }: { code: string; language: string }) {
  const tokens = useCodeTokens(code, language)
  return <pre className="highlighted-code"><code>{tokens === null ? code : tokens.map((line, row) => <span key={row}><CodeTokens tokens={line} fallback="" />{row < tokens.length - 1 ? '\n' : ''}</span>)}</code></pre>
})
