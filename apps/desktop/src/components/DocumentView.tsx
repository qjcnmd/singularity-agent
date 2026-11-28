import { CodeTokens, useCodeTokens } from '../highlight'
import { directoryOf } from '../workspace'
import { MarkdownBody, PathBase } from './MarkdownBody'

const maxHighlightedLength = 200_000

export function MarkdownDocumentView({ path, content }: { path: string; content: string }) {
  return <PathBase.Provider value={directoryOf(path)}>
    <div className="file-markdown timeline-body message-body"><MarkdownBody text={frontmatterAsCode(content)} /></div>
  </PathBase.Provider>
}

export function CodeView({ code, language }: { code: string; language: string }) {
  const normalized = code.replace(/\r\n/g, '\n')
  const tokens = useCodeTokens(normalized, normalized.length > maxHighlightedLength ? 'text' : language)
  const lines = normalized.split('\n')
  return <div className="file-code">{lines.map((line, row) => <div key={row} className="tool-line">
    <span className="tool-line-number">{row + 1}</span>
    <span>{tokens?.[row] ? <CodeTokens tokens={tokens[row]} /> : line}</span>
  </div>)}</div>
}

function frontmatterAsCode(content: string): string {
  return content.replace(/^---\r?\n([\s\S]*?)\r?\n---\r?\n/, (_, yaml: string) => '```yaml\n' + yaml + '\n```\n\n')
}
