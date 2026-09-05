let highlighterPromise: Promise<{
  codeToHtml(code: string, options: { lang: string; theme: string }): string
}> | null = null

export async function highlightCode(code: string, language: string): Promise<string> {
  highlighterPromise ??= Promise.all([
    import('@shikijs/core'),
    import('@shikijs/engine-javascript'),
    import('@shikijs/themes/github-dark'),
    import('@shikijs/langs/javascript'),
    import('@shikijs/langs/typescript'),
    import('@shikijs/langs/tsx'),
    import('@shikijs/langs/rust'),
    import('@shikijs/langs/json'),
    import('@shikijs/langs/markdown'),
    import('@shikijs/langs/bash'),
    import('@shikijs/langs/diff'),
  ]).then(async ([core, engine, theme, ...languages]) => {
    const highlighter = await core.createHighlighterCore({
      themes: [theme.default],
      langs: languages.flatMap((language) => language.default),
      engine: engine.createJavaScriptRegexEngine(),
    })
    return highlighter
  })
  const highlighter = await highlighterPromise
  const supported = new Set(['text', 'javascript', 'typescript', 'tsx', 'rust', 'json', 'markdown', 'bash', 'diff'])
  return highlighter.codeToHtml(code, {
    lang: supported.has(language) ? language : 'text',
    theme: 'github-dark',
  })
}
