type DiffLine = { kind: 'context' | 'added' | 'deleted'; number: number; text: string }
export type DiffHunk = { lines: DiffLine[] }

/** 解析后端保存的增删统计和 unified diff；行号来自补丁头，不从工具参数推测。 */
export function parseEditDiff(output: string) {
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
