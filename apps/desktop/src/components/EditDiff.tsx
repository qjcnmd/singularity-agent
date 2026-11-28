import { useMemo, useState } from 'react'
import { FileDiff } from 'lucide-react'
import { CodeTokens, useCodeTokens } from '../highlight'
import { useSelectionGuard } from '../interactions'
import type { ChangedFile } from '../protocol'
import { parseDiffSummary, type TurnFileEdit } from '../timeline'
import { normalizePath, pathBaseName, pathExtension, splitPathNameDir, workspaceRelativePath } from '../workspace'
import { CopyButton } from './CopyButton'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

type DiffLine = { kind: 'context' | 'added' | 'deleted'; number: number; text: string }
type DiffHunk = { lines: DiffLine[] }

function parseEditDiff(output: string) {
  const [summary, ...lines] = output.split('\n')
  const counts = parseDiffSummary(summary)
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
  return { added: counts.added, deleted: counts.deleted, hunks, truncated }
}

export function DiffCounts({ added, deleted, showZero = false, labelled = false }: {
  added: number
  deleted: number
  showZero?: boolean
  labelled?: boolean
}) {
  if (!showZero && added === 0 && deleted === 0) return null
  return (
    <span className="edit-counts">
      {(showZero || added > 0) && (
        <span className="edit-added-count" aria-label={labelled ? `新增 ${added} 行` : undefined}>+{added}</span>
      )}
      {(showZero || deleted > 0) && (
        <span className="edit-deleted-count" aria-label={labelled ? `删除 ${deleted} 行` : undefined}>-{deleted}</span>
      )}
    </span>
  )
}

export function EditDiff({ path, output, onOpenFile, bare = false }: { path: string; output: string; onOpenFile?: () => void; bare?: boolean }) {
  const diff = useMemo(() => parseEditDiff(output), [output])
  const filename = pathBaseName(path)
  const language = pathExtension(path) || 'text'
  return <div className={`edit-diff${bare ? ' is-bare' : ''}`}>
      {!bare && <div className="edit-diff-header">
        {onOpenFile
          ? <button type="button" className="edit-filename edit-filename-button" title={`查看 ${path} 合并差异`} onClick={onOpenFile}>{filename}</button>
          : <span className="edit-filename" title={path}>{filename}</span>}
        {diff && <DiffCounts added={diff.added} deleted={diff.deleted} showZero labelled />}
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
    return <DiffCounts added={file.added} deleted={file.deleted} />
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
    const normalized = normalizePath(edit.path).trim()
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
    const { name, dir } = splitPathNameDir(relPath)
    byKey.set(key, {
      rawPath: edit.path,
      relPath,
      inWorkspace,
      name,
      dir,
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
          <DiffCounts added={totalAdded} deleted={totalDeleted} />
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
  const { oldText, newText, indexed } = useMemo(() => {
    const oldLines: string[] = []
    const newLines: string[] = []
    const entries: Array<{ line: DiffLine; tokenSource: 'old' | 'new'; tokenIndex: number }> = []
    for (const line of hunk.lines) {
      if (line.kind === 'deleted') {
        entries.push({ line, tokenSource: 'old', tokenIndex: oldLines.length })
        oldLines.push(line.text)
      } else {
        if (line.kind === 'context') oldLines.push(line.text)
        entries.push({ line, tokenSource: 'new', tokenIndex: newLines.length })
        newLines.push(line.text)
      }
    }
    return { oldText: oldLines.join('\n'), newText: newLines.join('\n'), indexed: entries }
  }, [hunk.lines])
  const oldTokens = useCodeTokens(oldText, language)
  const newTokens = useCodeTokens(newText, language)
  return <div className="edit-hunk">{indexed.map(({ line, tokenSource, tokenIndex }, index) => {
    const tokens = tokenSource === 'old' ? oldTokens?.[tokenIndex] : newTokens?.[tokenIndex]
    return <div key={index} className={`edit-line edit-line-${line.kind}`}>
      <span className="edit-line-number" aria-label={`${line.kind === 'deleted' ? '删除的原' : line.kind === 'added' ? '新增的' : ''}行 ${line.number}`}>{line.number}</span>
      <code>{tokens ? <CodeTokens tokens={tokens} /> : line.text || '\u00a0'}</code>
    </div>
  })}</div>
}
