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

let highlighterPromise: Promise<{
  codeToTokens(code: string, options: { lang: string; theme: string }): { tokens: Array<Array<{content: string; color?: string; darkColor?: string; fontStyle?: number}>> }
}> | null = null

export async function highlightCode(code: string, language: string): Promise<Array<Array<{content: string; color?: string; darkColor?: string; fontStyle?: number}>>> {
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
  const highlighter = await highlighterPromise
  const supported = new Set<string>([...Object.keys(LANGUAGE_LOADERS), 'text'])
  const lang = supported.has(language) ? language : 'text'
  const light = highlighter.codeToTokens(code, { lang, theme: 'github-light' }).tokens
  const dark = highlighter.codeToTokens(code, { lang, theme: 'github-dark' }).tokens
  return light.map((line, row) => line.map((token, column) => ({ ...token, darkColor: dark[row]?.[column]?.color })))
}
