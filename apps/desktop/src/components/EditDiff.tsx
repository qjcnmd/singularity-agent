import { useMemo } from 'react'
import { CodeTokens, useCodeTokens } from '../highlight'
import { CopyButton } from './CopyButton'
import '../styles/edit-diff.css'

type DiffLine = { kind: 'context' | 'added' | 'deleted'; number: number; text: string }
type DiffHunk = { lines: DiffLine[] }

/** 解析后端保存的增删统计和 unified diff；行号来自补丁头，不从工具参数推测。 */
function parseEditDiff(output: string) {
  const [summary, ...lines] = output.split('\n')
  const counts = /^\+(\d+) -(\d+)$/.exec(summary)
  if (!counts) return null
  let oldLine = 0
  let newLine = 0
  let truncated = false
  const hunks: DiffHunk[] = []
  for (const line of lines) {
    const header = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line)
    if (header) {
      oldLine = Number(header[1])
      newLine = Number(header[2])
      hunks.push({ lines: [] })
      continue
    }
    if (line === '[Diff truncated]') { truncated = true; continue }
    if (line === '' || line.startsWith('\\')) continue
    const hunk = hunks.at(-1)
    if (!hunk) return null
    if (line[0] === '+') hunk.lines.push({ kind: 'added', number: newLine++, text: line.slice(1) })
    else if (line[0] === '-') hunk.lines.push({ kind: 'deleted', number: oldLine++, text: line.slice(1) })
    else if (line[0] === ' ') {
      hunk.lines.push({ kind: 'context', number: newLine++, text: line.slice(1) })
      oldLine++
    } else return null
  }
  return { added: Number(counts[1]), deleted: Number(counts[2]), hunks, truncated }
}

export function EditDiff({ path, output }: { path: string; output: string }) {
  const diff = useMemo(() => parseEditDiff(output), [output])
  const filename = path.replaceAll('\\', '/').split('/').at(-1)
  const language = /\.([\w-]+)$/.exec(path)?.[1] ?? 'text'
  return <div className="edit-diff">
      <div className="edit-diff-header">
        <span className="edit-filename" title={path}>{filename}</span>
        {diff && <span className="edit-counts"><span className="edit-added-count" aria-label={`新增 ${diff.added} 行`}>+{diff.added}</span><span className="edit-deleted-count" aria-label={`删除 ${diff.deleted} 行`}>-{diff.deleted}</span></span>}
        <CopyButton text={output.split('\n').slice(1).join('\n')} label="复制差异" iconOnly />
      </div>
      <div className="edit-diff-scroll" tabIndex={0} role="region" aria-label={`${filename} 的代码差异`}>
        {diff ? diff.hunks.map((hunk, index) => <Hunk key={index} hunk={hunk} language={language} />) : <pre>{output}</pre>}
      </div>
      {diff?.truncated && <p className="edit-diff-note">差异内容较长，仅显示前面的部分。文件修改已全部完成。</p>}
  </div>
}

function Hunk({ hunk, language }: { hunk: DiffHunk; language: string }) {
  const oldTokens = useCodeTokens(hunk.lines.filter(line => line.kind !== 'added').map(line => line.text).join('\n'), language)
  const newTokens = useCodeTokens(hunk.lines.filter(line => line.kind !== 'deleted').map(line => line.text).join('\n'), language)
  let oldIndex = 0
  let newIndex = 0
  return <div className="edit-hunk">{hunk.lines.map((line, index) => {
    if (line.kind === 'context') oldIndex++
    const tokens = line.kind === 'deleted' ? oldTokens?.[oldIndex++] : newTokens?.[newIndex++]
    return <div key={index} className={`edit-line edit-line-${line.kind}`}>
      <span className="edit-line-number" aria-label={`${line.kind === 'deleted' ? '删除的原' : line.kind === 'added' ? '新增的' : ''}行 ${line.number}`}>{line.number}</span>
      <code>{tokens ? <CodeTokens tokens={tokens} /> : line.text || '\u00a0'}</code>
    </div>
  })}</div>
}
