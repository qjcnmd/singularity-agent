let highlighterPromise: Promise<{
  codeToTokens(code: string, options: { lang: string; theme: string }): { tokens: Array<Array<{content: string; color?: string; darkColor?: string; fontStyle?: number}>> }
}> | null = null

export async function highlightCode(code: string, language: string): Promise<Array<Array<{content: string; color?: string; darkColor?: string; fontStyle?: number}>>> {
  highlighterPromise ??= Promise.all([
    import('@shikijs/core'),
    import('@shikijs/engine-javascript'),
    import('@shikijs/themes/github-light'),
    import('@shikijs/themes/github-dark'),
    import('@shikijs/langs/javascript'),
    import('@shikijs/langs/typescript'),
    import('@shikijs/langs/tsx'),
    import('@shikijs/langs/rust'),
    import('@shikijs/langs/json'),
    import('@shikijs/langs/markdown'),
    import('@shikijs/langs/bash'),
    import('@shikijs/langs/diff'),
  ]).then(async ([core, engine, theme, darkTheme, ...languages]) => {
    const highlighter = await core.createHighlighterCore({
      themes: [theme.default, darkTheme.default],
      langs: languages.flatMap((language) => language.default),
      engine: engine.createJavaScriptRegexEngine(),
    })
    return highlighter
  })
  const highlighter = await highlighterPromise
  const supported = new Set(['text', 'javascript', 'typescript', 'tsx', 'rust', 'json', 'markdown', 'bash', 'diff'])
  const lang = supported.has(language) ? language : 'text'
  const light = highlighter.codeToTokens(code, { lang, theme: 'github-light' }).tokens
  const dark = highlighter.codeToTokens(code, { lang, theme: 'github-dark' }).tokens
  return light.map((line, row) => line.map((token, column) => ({ ...token, darkColor: dark[row]?.[column]?.color })))
}
