/**
 * 代码高亮：Shiki 引擎、明暗两套主题的 token，以及渲染 token 的 React hook。
 * Markdown 代码块从这里取高亮结果。
 */
import { createElement, useEffect, useMemo, useState, type CSSProperties } from 'react'

/**
 * 支持高亮的语言。每个语言的 import 必须写成静态字符串，打包时才会拆成单独的分块，
 * 所以语言名和加载函数写在同一个对象里；语言清单直接取对象的键，不另写一份。
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

type LanguageId = keyof typeof LANGUAGE_LOADERS
const LANGUAGE_ALIASES: Record<string, LanguageId> = {
  js: 'javascript', jsx: 'javascript', ts: 'typescript', rs: 'rust', md: 'markdown', sh: 'bash',
}

/** 将围栏语言名或文件扩展名解析为已加载语言；未知语言保留纯文本。 */
function languageIdFor(language: string): LanguageId | 'text' {
  const name = language.trim().split(/\s+/)[0].toLowerCase()
  if (Object.hasOwn(LANGUAGE_LOADERS, name)) return name as LanguageId
  return Object.hasOwn(LANGUAGE_ALIASES, name) ? LANGUAGE_ALIASES[name] : 'text'
}

/** 高亮器类型直接取自库的导出，不另写一份简化的类型。 */
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
  ]).then(([core, engine, theme, darkTheme]) => core.createHighlighterCore({
    themes: [theme.default, darkTheme.default],
    langs: [],
    engine: engine.createJavaScriptRegexEngine(),
  }))
  return highlighterPromise
}

// 每种实际使用的语言只加载一次；普通文本不会初始化高亮引擎。
const languagePromises = new Map<LanguageId, Promise<Highlighter>>()
function loadLanguage(language: LanguageId): Promise<Highlighter> {
  let pending = languagePromises.get(language)
  if (!pending) {
    pending = Promise.all([loadHighlighter(), LANGUAGE_LOADERS[language]()]).then(async ([highlighter, grammar]) => {
      await highlighter.loadLanguage(...grammar.default)
      return highlighter
    })
    languagePromises.set(language, pending)
  }
  return pending
}

function tokenize(highlighter: Highlighter, code: string, lang: LanguageId): HighlightedLines {
  // 明暗两套配色由库一次给出；defaultColor: false 让样式只保留 CSS 变量，
  // 由样式表按当前主题选用其中之一。
  return highlighter.codeToTokens(code, {
    lang,
    themes: { light: 'github-light', dark: 'github-dark' },
    defaultColor: false,
  }).tokens
}

/** 单行高亮 token 的库类型。 */
type CodeLine = HighlightedLines[number]

/** 状态里只记录语言是否加载完成；token 每次渲染时按当前代码计算，不在 Promise 回调里写回。
 *  这样流式输出改变代码时，不会出现“高亮完成 → setState → 再次渲染”的连锁更新。 */
export function useCodeTokens(code: string, language: string) {
  const lang = languageIdFor(language)
  const [loaded, setLoaded] = useState<{ highlighter: Highlighter; language: LanguageId } | null>(null)
  useEffect(() => {
    if (lang === 'text') return
    let current = true
    void loadLanguage(lang).then(
      highlighter => { if (current) setLoaded({ highlighter, language: lang }) },
      // 语言分块或高亮失败：保持 null，落回既有原文展示，不留下未处理的拒绝。
      () => {},
    )
    return () => { current = false }
  }, [lang])
  return useMemo(() => {
    if (loaded === null || loaded.language !== lang) return null
    try { return tokenize(loaded.highlighter, code, lang) }
    catch { return null } // 高亮失败时仍显示原文，与异步加载失败保持同一行为。
  }, [loaded, code, lang])
}

/** 高亮就绪后的单行 token。 */
export function CodeTokens({ tokens }: { tokens: CodeLine }) {
  return tokens.map((token, column) => createElement('span', {
    key: column,
    className: 'code-token',
    style: {
      ...token.htmlStyle,
      fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined,
      fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined,
    } as CSSProperties,
  }, token.content))
}
