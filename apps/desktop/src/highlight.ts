/**
 * 代码高亮：按需加载 Shiki 语言语法并生成支持明暗双主题的 Token。
 */
import { createElement, useEffect, useMemo, useState, type ReactNode, type CSSProperties } from 'react'

/**
 * 支持高亮的语言加载表；使用静态 `import()` 让打包器按语言拆包。
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

/** 将代码块标记或扩展名映射为支持的语言 ID，不支持的语言返回 `'text'`。 */
function languageIdFor(language: string): LanguageId | 'text' {
  const name = language.trim().split(/\s+/)[0].toLowerCase()
  if (Object.hasOwn(LANGUAGE_LOADERS, name)) return name as LanguageId
  return Object.hasOwn(LANGUAGE_ALIASES, name) ? LANGUAGE_ALIASES[name] : 'text'
}

type Highlighter = Awaited<ReturnType<typeof import('@shikijs/core')['createHighlighterCore']>>

/** 每个 Token 同时带有 `--shiki-light` 和 `--shiki-dark` CSS 变量。 */
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
      // 语言分块加载失败时保持 null，回退为纯文本展示。
      () => {},
    )
    return () => { current = false }
  }, [lang])
  return useMemo(() => {
    if (loaded === null || loaded.language !== lang) return null
    try { return tokenize(loaded.highlighter, code, lang) }
    catch { return null } // 分词失败时回退为纯文本展示。
  }, [loaded, code, lang])
}

export function CodeTokens({ tokens, renderText }: { tokens: CodeLine; renderText?: (text: string) => ReactNode }) {
  return tokens.map((token, column) => createElement('span', {
    key: column,
    className: 'code-token',
    style: {
      ...token.htmlStyle,
      fontStyle: (token.fontStyle ?? 0) & 1 ? 'italic' : undefined,
      fontWeight: (token.fontStyle ?? 0) & 2 ? 'bold' : undefined,
    } as CSSProperties,
  }, renderText ? renderText(token.content) : token.content))
}
