import { useMemo, useState } from 'react'
import { FileDiff } from 'lucide-react'
import { CodeTokens, useCodeTokens } from '../highlight'
import { useSelectionGuard } from '../interactions'
import type { ChangedFile } from '../protocol'
import type { TurnFileEdit } from '../timeline'
import { workspaceRelativePath } from '../workspace'
import { CopyButton } from './CopyButton'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

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

export function EditDiff({ path, output, onOpenFile, bare = false }: { path: string; output: string; onOpenFile?: () => void; bare?: boolean }) {
  const diff = useMemo(() => parseEditDiff(output), [output])
  const filename = path.replaceAll('\\', '/').split('/').at(-1) ?? path
  const language = /\.([\w-]+)$/.exec(path)?.[1] ?? 'text'
  return <div className={`edit-diff${bare ? ' is-bare' : ''}`}>
      {!bare && <div className="edit-diff-header">
        {onOpenFile
          ? <button type="button" className="edit-filename edit-filename-button" title={`查看 ${path} 合并差异`} onClick={onOpenFile}>{filename}</button>
          : <span className="edit-filename" title={path}>{filename}</span>}
        {diff && <span className="edit-counts"><span className="edit-added-count" aria-label={`新增 ${diff.added} 行`}>+{diff.added}</span><span className="edit-deleted-count" aria-label={`删除 ${diff.deleted} 行`}>-{diff.deleted}</span></span>}
        {diff && <CopyButton text={output.split('\n').slice(1).join('\n')} label="复制差异" iconOnly />}
      </div>}
      <div className="edit-diff-scroll" tabIndex={0} role="region" aria-label={`${filename} 的代码差异`}>
        {diff ? diff.hunks.map((hunk, index) => <Hunk key={index} hunk={hunk} language={language} />) : <pre className="edit-diff-fallback">{output}</pre>}
      </div>
      {diff?.truncated && <p className="edit-diff-note">差异内容较长，仅显示前面的部分。文件修改已全部完成。</p>}
  </div>
}

export function FileChangeStats({ file }: { file: Pick<ChangedFile, 'added' | 'deleted' | 'status'> }) {
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

interface SummarizedTurnFile {
  rawPath: string
  relPath: string
  inWorkspace: boolean
  name: string
  dir: string
  added: number
  deleted: number
  status: 'added' | 'modified'
}

function summarizeTurnFiles(edits: TurnFileEdit[], cwd: string): SummarizedTurnFile[] {
  const byKey = new Map<string, SummarizedTurnFile>()
  for (const edit of edits) {
    const normalized = edit.path.replaceAll('\\', '/').trim()
    const rel = workspaceRelativePath(edit.path, cwd)
    const inWorkspace = rel !== null
    const relPath = rel ?? normalized
    const key = relPath.toLowerCase()
    const existing = byKey.get(key)
    if (existing) {
      existing.added += edit.added
      existing.deleted += edit.deleted
      continue
    }
    const slash = relPath.lastIndexOf('/')
    byKey.set(key, {
      rawPath: edit.path,
      relPath,
      inWorkspace,
      name: slash >= 0 ? relPath.slice(slash + 1) : relPath,
      dir: slash >= 0 ? relPath.slice(0, slash) : '',
      added: edit.added,
      deleted: edit.deleted,
      status: edit.created ? 'added' : 'modified',
    })
  }
  return [...byKey.values()]
}

export function TurnChangesCard({ edits, cwd, itemKey, onOpenChanges, onOpenFile, onDisclosureChange }: {
  edits: TurnFileEdit[]
  cwd: string
  itemKey: string
  onOpenChanges?: (focusPath?: string) => void
  onOpenFile: (path: string) => void
  onDisclosureChange: (element: HTMLElement) => void
}) {
  const files = useMemo(() => summarizeTurnFiles(edits, cwd), [edits, cwd])
  const [open, setOpen] = useState(() => files.length <= 8)
  const guard = useSelectionGuard(onDisclosureChange)
  const totalAdded = files.reduce((sum, file) => sum + file.added, 0)
  const totalDeleted = files.reduce((sum, file) => sum + file.deleted, 0)
  if (files.length === 0) return null
  return (
    <section className="timeline-item turn-changes-card" data-item-id={itemKey} aria-label={`本轮改动 ${files.length} 个文件`}>
      <div className="turn-changes-head">
        <button type="button" className="turn-changes-toggle" aria-expanded={open} {...guard(() => setOpen(value => !value))}>
          <span className="turn-changes-title">本轮改动 {files.length} 个文件</span>
          {(totalAdded > 0 || totalDeleted > 0) && (
            <span className="edit-counts">
              {totalAdded > 0 && <span className="edit-added-count">+{totalAdded}</span>}
              {totalDeleted > 0 && <span className="edit-deleted-count">-{totalDeleted}</span>}
            </span>
          )}
          <ExpandChevron expanded={open} size={13} />
        </button>
        {onOpenChanges && (
          <button type="button" className="turn-changes-review" onClick={() => onOpenChanges()}>
            <FileDiff size={13} strokeWidth={1.8} aria-hidden="true" />
            <span>查看改动</span>
          </button>
        )}
      </div>
      <Disclosure open={open}>
        <div className="turn-changes-list">
          {files.map(file => (
            <button
              key={file.inWorkspace ? file.relPath : file.rawPath}
              type="button"
              className="turn-changes-row"
              title={file.inWorkspace ? file.relPath : file.rawPath}
              onClick={() => {
                if (file.inWorkspace && onOpenChanges) onOpenChanges(file.relPath)
                else onOpenFile(file.rawPath)
              }}
            >
              <span className={`changes-status is-${file.status}`} aria-hidden="true">{file.status === 'added' ? 'A' : 'M'}</span>
              <span className="changes-path">
                <span className="changes-name">{file.name}</span>
                {file.dir && <span className="changes-dir">{file.dir}</span>}
              </span>
              <FileChangeStats file={file} />
            </button>
          ))}
        </div>
      </Disclosure>
    </section>
  )
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
