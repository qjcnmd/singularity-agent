import type { parsePatch } from 'diff'

type Hunk = ReturnType<typeof parsePatch>[number]['hunks'][number]

/** Keep one context line on either side of each changed block, preserving source numbers. */
export function diffContext(hunks: Hunk[]): Hunk[] {
  return hunks.flatMap(hunk => {
    const keep = hunk.lines.map(line => line[0] === '+' || line[0] === '-')
    for (let index = 0; index < hunk.lines.length; index++) {
      if (hunk.lines[index][0] !== '+' && hunk.lines[index][0] !== '-') continue
      for (const direction of [-1, 1]) {
        let adjacent = index + direction
        while (hunk.lines[adjacent]?.[0] === '\\') { keep[adjacent] = true; adjacent += direction }
        if (hunk.lines[adjacent]?.[0] === ' ') keep[adjacent] = true
      }
    }
    let oldLine = hunk.oldStart, newLine = hunk.newStart
    const result: Hunk[] = []
    let current: Hunk | undefined
    hunk.lines.forEach((line, index) => {
      const marker = line[0]
      if (marker === '\\' && index > 0 && keep[index - 1]) keep[index] = true
      if (keep[index]) {
        if (current === undefined) {
          current = { oldStart: oldLine, oldLines: 0, newStart: newLine, newLines: 0, lines: [] }
          result.push(current)
        }
        current.lines.push(line)
        if (marker === '-' || marker === ' ') current.oldLines++
        if (marker === '+' || marker === ' ') current.newLines++
      } else current = undefined
      if (marker === '-' || marker === ' ') oldLine++
      if (marker === '+' || marker === ' ') newLine++
    })
    return result
  })
}
