/**
 * 为 `read` 工具输出的前 `source.lineCount` 行标注源文件行号；
 * 超出部分的截断或分页提示不标行号。
 */

import type { ReadSource } from './protocol'

interface ReadOutputLine {
  text: string
  /** 源文件行号；尾部分页或截断说明为 `undefined`。 */
  number?: number
}

export function readOutputLines(text: string, source: ReadSource): ReadOutputLine[] {
  const lines = text.replace(/\r\n/g, '\n').split('\n')
  return lines.map((line, index) => index < source.lineCount
    ? { text: line, number: source.startLine + index }
    : { text: line })
}
