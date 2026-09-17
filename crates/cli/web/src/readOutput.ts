/** read 工具输出的行编号。
 *
 * 后端的两种尾部说明（分页续读、超长单行）都追加在正文之后，并与正文隔一个空行。
 * 只有这个完整形状才被当作说明：文字必须与后端当前格式逐字匹配，且编号与 offset、
 * 正文行数一致。正文里恰好出现同样文字仍按源文件行编号，说明本身也不会冒充文件行。
 */

interface ReadOutputLine {
  text: string
  /** 源文件行号；尾部说明与分隔空行为 undefined。 */
  number?: number
}

/** 分页续读说明：给出的起止行号与下一页 offset 都由正文行数决定。 */
const paginationNote = /^\[Showing lines (\d+)-(\d+)\. File continues; use offset=(\d+) to continue\.\]$/
/** 超长单行说明：正文只有起始行这一行，下一页从它之后开始。 */
const incompleteLineNote = /^\[Line (\d+) exceeds \d+KB; only its prefix is shown\. Use bash to read this line in byte ranges\. For following lines use offset=(\d+)\.\]$/

export function readOutputLines(text: string, startLine: number): ReadOutputLine[] {
  const lines = text.replace(/\r\n/g, '\n').replace(/\n$/, '').split('\n')
  const content = contentLineCount(lines, startLine)
  return lines.map((line, index) => index < content
    ? { text: line, number: startLine + index }
    : { text: line })
}

/** 正文行数：仅当末段是本后端生成的完整说明时，把它与分隔空行排除在编号之外。 */
function contentLineCount(lines: string[], startLine: number): number {
  const separator = lines.length - 2
  if (separator < 1 || lines[separator] !== '') return lines.length
  const count = separator
  const lastLine = startLine + count - 1
  const note = lines[separator + 1]
  const page = paginationNote.exec(note)
  if (page !== null && Number(page[1]) === startLine && Number(page[2]) === lastLine && Number(page[3]) === lastLine + 1) return count
  const incomplete = incompleteLineNote.exec(note)
  if (incomplete !== null && count === 1 && Number(incomplete[1]) === startLine && Number(incomplete[2]) === startLine + 1) return count
  return lines.length
}
