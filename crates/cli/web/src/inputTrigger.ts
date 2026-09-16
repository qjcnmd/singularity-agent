/** 检测光标处的 command/file token，排除路径与 URL。 */
export function inputTrigger(text: string, caret: number): { kind: 'skill' | 'file'; start: number; end: number; query: string } | null {
  for (let start = caret - 1; start >= 0; start--) {
    const marker = text[start]
    if (/\s/u.test(marker)) break
    if (marker !== '/' && marker !== '@') continue
    const previous = text[start - 1]
    if (previous && /[\p{L}\p{N}_]/u.test(previous)) continue
    // `/` 开头的技能触发只认真正的词首斜杠：目录分隔符（`./`、`~/`、盘符
    // 路径、`//`、网址里的 `://`）都是路径，随便打一个相对路径不该弹出技能
    // 选单。`@` 触发不受影响——那本来就是文件引用的入口。
    if (marker === '/' && (previous === '/' || previous === '.' || previous === '~' || text[start + 1] === '/'
      || previous === ':' && start > 1 && !/\s/u.test(text[start - 2]))) continue
    return { kind: marker === '/' ? 'skill' : 'file', start, end: caret, query: text.slice(start + 1, caret) }
  }
  return null
}
