import { useMemo } from 'react'
import { parseEditDiff, type DiffHunk } from '../editDiff'
import { CodeTokens, useCodeTokens } from '../highlight'
import { CopyButton } from './CopyButton'
import '../styles/edit-diff.css'

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
  const before = hunk.lines.filter(line => line.kind !== 'added')
  const after = hunk.lines.filter(line => line.kind !== 'deleted')
  const oldTokens = useCodeTokens(before.map(line => line.text).join('\n'), language)
  const newTokens = useCodeTokens(after.map(line => line.text).join('\n'), language)
  const highlighted = new Map([
    ...before.map((line, index) => [line, oldTokens?.[index]] as const),
    ...after.map((line, index) => [line, newTokens?.[index]] as const),
  ])
  return <div className="edit-hunk">{hunk.lines.map((line, index) => {
    const tokens = highlighted.get(line)
    return <div key={index} className={`edit-line edit-line-${line.kind}`}>
    <span className="edit-line-number" aria-label={`${line.kind === 'deleted' ? '删除的原' : line.kind === 'added' ? '新增的' : ''}行 ${line.number}`}>{line.number}</span>
    <code>{tokens ? <CodeTokens tokens={tokens} /> : line.text || '\u00a0'}</code>
  </div>})}</div>
}
