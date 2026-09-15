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

export async function highlightCode(code: string, language: string): Promise<HighlightedLines> {
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
  const lang = Object.hasOwn(LANGUAGE_LOADERS, language) ? language : 'text'
  // 明暗两套配色由库一次给出；defaultColor: false 让样式只保留 CSS 变量，
  // 由样式表按当前主题选用其中之一。
  return highlighter.codeToTokens(code, {
    lang,
    themes: { light: 'github-light', dark: 'github-dark' },
    defaultColor: false,
  }).tokens
}
