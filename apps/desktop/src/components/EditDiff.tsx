import { useMemo } from 'react'
import { CodeTokens, useCodeTokens } from '../highlight'
import type { ChangedFile } from '../protocol'
import { CopyButton } from './CopyButton'
import '../styles/edit-diff.css'

type DiffLine = { kind: 'context' | 'added' | 'deleted'; number: number; text: string }
type DiffHunk = { lines: DiffLine[] }

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

export function EditDiff({ path, output, onOpenFile }: { path: string; output: string; onOpenFile?: () => void }) {
  const diff = useMemo(() => parseEditDiff(output), [output])
  const filename = path.replaceAll('\\', '/').split('/').at(-1) ?? path
  const language = /\.([\w-]+)$/.exec(path)?.[1] ?? 'text'
  return <div className="edit-diff">
      <div className="edit-diff-header">
        {onOpenFile
          ? <button type="button" className="edit-filename edit-filename-button" title={`查看 ${path} 合并差异`} onClick={onOpenFile}>{filename}</button>
          : <span className="edit-filename" title={path}>{filename}</span>}
        {diff && <span className="edit-counts"><span className="edit-added-count" aria-label={`新增 ${diff.added} 行`}>+{diff.added}</span><span className="edit-deleted-count" aria-label={`删除 ${diff.deleted} 行`}>-{diff.deleted}</span></span>}
        {diff && <CopyButton text={output.split('\n').slice(1).join('\n')} label="复制差异" iconOnly />}
      </div>
      <div className="edit-diff-scroll" tabIndex={0} role="region" aria-label={`${filename} 的代码差异`}>
        {diff ? diff.hunks.map((hunk, index) => <Hunk key={index} hunk={hunk} language={language} />) : <pre className="edit-diff-fallback">{output}</pre>}
      </div>
      {diff?.truncated && <p className="edit-diff-note">差异内容较长，仅显示前面的部分。文件修改已全部完成。</p>}
  </div>
}

export function FileChangeStats({ file }: { file: ChangedFile }) {
  if (file.added > 0 || file.deleted > 0) {
    return (
      <span className="edit-counts">
        {file.added > 0 && <span className="edit-added-count">+{file.added}</span>}
        {file.deleted > 0 && <span className="edit-deleted-count">-{file.deleted}</span>}
      </span>
    )
  }
  const label = file.status === 'added' ? '新增' : file.status === 'deleted' ? '删除' : '修改'
  return <span className={`changed-file-badge is-${file.status}`}>{label}</span>
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
