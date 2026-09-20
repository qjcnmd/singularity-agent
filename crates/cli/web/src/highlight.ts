/**
 * 公共代码高亮：Shiki 引擎、明暗双主题 token，以及消费 token 的 React 绑定。
 * Markdown 代码块与 Diff 都从这里取高亮结果，不各自持有或从对方转发。
 *
 * 本模块保持 .ts：Node 的测试运行器直接加载它断言双主题输出，而类型剥离不支持
 * .tsx/JSX，因此唯一的 span 用 createElement 构造；这里没有其他 JSX 布局。
 */
import { createElement, useEffect, useMemo, useState, type CSSProperties } from 'react'

/**
 * 支持高亮的语言。静态 import 说明符是打包分块边界，语言名与加载器必须成对出现，
 * 因此这里用一个对象同时给出两者：语言清单由对象键派生，不再另写一份名单。
 */
const LANGUAGE_LOADERS = {
  javascript: () => import('@shikijs/langs/javascript'),
  typescript: () => import('@shikijs/langs/typescript'),
  tsx: () => import('@shikijs/langs/tsx'),
  rust: () => import('@shikijs/langs/rust'),
  json: () => import('@shikijs/langs/json'),
  markdown: () => import('@shikijs/langs/markdown'),
  bash: () => import('@shikijs/langs/bash'),
  diff: () => import('@shikijs/langs/diff'),
} as const

/** 高亮器契约直接取自库的导出签名，应用不再手写一份缩小的 token 结构。 */
type Highlighter = Awaited<ReturnType<typeof import('@shikijs/core')['createHighlighterCore']>>

/** 每个 token 携带明暗两套配色（库的 --shiki-light/--shiki-dark CSS 变量）。 */
type HighlightedLines = ReturnType<Highlighter['codeToTokens']>['tokens']

let highlighterPromise: Promise<Highlighter> | null = null

function loadHighlighter(): Promise<Highlighter> {
  highlighterPromise ??= Promise.all([
    import('@shikijs/core'),
    import('@shikijs/engine-javascript'),
    import('@shikijs/themes/github-light'),
    import('@shikijs/themes/github-dark'),
    ...Object.values(LANGUAGE_LOADERS).map((load) => load()),
  ]).then(async ([core, engine, theme, darkTheme, ...languages]) => {
    const highlighter = await core.createHighlighterCore({
      themes: [theme.default, darkTheme.default],
      langs: languages.flatMap((language) => language.default),
      engine: engine.createJavaScriptRegexEngine(),
    })
    return highlighter
  })
  return highlighterPromise
}

function tokenize(highlighter: Highlighter, code: string, language: string): HighlightedLines {
  const lang = Object.hasOwn(LANGUAGE_LOADERS, language) ? language : 'text'
  // 明暗两套配色由库一次给出；defaultColor: false 让样式只保留 CSS 变量，
  // 由样式表按当前主题选用其中之一。
  return highlighter.codeToTokens(code, {
    lang,
    themes: { light: 'github-light', dark: 'github-dark' },
    defaultColor: false,
  }).tokens
}

export async function highlightCode(code: string, language: string): Promise<HighlightedLines> {
  return tokenize(await loadHighlighter(), code, language)
}

/** 单行高亮 token 的库类型；两个消费者只通过下面的 hook/渲染器使用它。 */
type CodeLine = HighlightedLines[number]

/** 只把异步资源就绪存入状态；token 由当前代码派生，不再经 Promise 写回状态。
 *  流式代码变化因此不会形成高亮完成 → setState → 再次提交的更新链。 */
export function useCodeTokens(code: string, language: string) {
  const [highlighter, setHighlighter] = useState<Highlighter | null>(null)
  useEffect(() => {
    let current = true
    void loadHighlighter().then(
      loaded => { if (current) setHighlighter(loaded) },
      // 语言分块或高亮失败：保持 null，落回既有原文展示，不留下未处理的拒绝。
      () => {},
    )
    return () => { current = false }
  }, [])
  return useMemo(() => {
    if (highlighter === null) return null
    try { return tokenize(highlighter, code, language) }
    catch { return null } // 高亮失败时仍显示原文，与异步加载失败保持同一行为。
  }, [highlighter, code, language])
}

/** 单行高亮 token；`undefined` 表示该行还没有高亮结果，调用方落回原文。 */
export function CodeTokens({ tokens, fallback }: { tokens?: CodeLine; fallback: string }) {
  return tokens === undefined ? fallback : tokens.map((token, column) => createElement('span', {
    key: column,
    className: 'code-token',
    style: {
      ...token.htmlStyle,
      fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined,
      fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined,
    } as CSSProperties,
  }, token.content))
}
