/** 技能只在开头或空白后触发；文件引用沿用原有词首规则。 */
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
