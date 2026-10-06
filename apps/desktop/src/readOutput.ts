/** read 工具输出的行编号。
 *
 * 起始行与正文行数来自 producer 记录的真实读取范围：尾部说明（分页续读、超长
 * 单行）只是正文之后的剩余文本，不再按文案匹配，也不会冒充源文件行。旧记录没有
 * 来源信息时调用方按普通文本展示，不猜正文/说明边界。
 */

import type { ReadSource } from './protocol'

interface ReadOutputLine {
  text: string
  /** 源文件行号；尾部说明与分隔空行为 undefined。 */
  number?: number
}

export function readOutputLines(text: string, source: ReadSource): ReadOutputLine[] {
  const lines = text.replace(/\r\n/g, '\n').replace(/\n$/, '').split('\n')
  return lines.map((line, index) => index < source.lineCount
    ? { text: line, number: source.startLine + index }
    : { text: line })
}
