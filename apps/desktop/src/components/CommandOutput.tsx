import Anser from 'anser'

/** Bash 调用与后台命令共用命令和 ANSI 输出显示。 */
export function CommandOutput({ command, output }: { command: string; output: string }) {
  return <div>
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <>
      <div className="tool-output-header">输出</div>
      <pre>{Anser.ansiToJson(output, { remove_empty: true }).map((part, index) => <span key={index} style={{
        color: part.fg ? `rgb(${part.fg})` : undefined,
        backgroundColor: part.bg ? `rgb(${part.bg})` : undefined,
        fontWeight: part.decorations.includes('bold') ? 700 : undefined,
      }}>{part.content}</span>)}</pre>
    </>}
  </div>
}
