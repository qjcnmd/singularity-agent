/** 识别光标前的 `/`：仅在行首或空白后、且后面不含路径分隔符时触发技能候选。 */
export function inputTrigger(text: string, caret: number): { start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/') continue
    const previous = text[start - 1]
    if (previous && !/\s/u.test(previous) || /[\/\\]/u.test(text.slice(start + 1, caret))) continue
    return { start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}
