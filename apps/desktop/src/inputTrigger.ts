/** 识别光标前的 `/`（仅在行首或空白后且不含路径分隔符时触发技能候选）或 `@`（非词中位置触发文件候选）。 */
export function inputTrigger(text: string, caret: number): { kind: 'skill' | 'file'; start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/' && marker !== '@') continue
    const previous = text[start - 1]
    if (previous && /[\p{L}\p{N}_]/u.test(previous)) continue
    if (marker === '/' && (previous && !/\s/u.test(previous) || /[\/\\]/u.test(text.slice(start + 1, caret)))) continue
    return { kind: marker === '/' ? 'skill' : 'file', start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}
