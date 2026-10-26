import Anser from 'anser'
import { CopyButton } from './CopyButton'

/** Bash 调用与后台命令共用命令和 ANSI 输出显示。 */
export function CommandOutput({ command, output }: { command: string; output: string }) {
  return <div className="command-output">
    <div className="command-output-header"><span>Bash</span><CopyButton text={output ? `${command}\n\n${output}` : command} label="复制命令与输出" /></div>
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <>
      <pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{
        color: part.fg ? `rgb(${part.fg})` : undefined,
        backgroundColor: part.bg ? `rgb(${part.bg})` : undefined,
        fontWeight: part.decorations.includes('bold') ? 700 : undefined,
      }}>{part.content}</span>)}</pre>
    </>}
  </div>
}
