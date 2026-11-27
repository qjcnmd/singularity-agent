import { CopyButton } from './CopyButton'

export function CommandOutput({ command, output }: { command: string; output: string }) {
  return <div className="command-output">
    <div className="command-output-header"><span>Bash</span><CopyButton text={output ? `${command}\n\n${output}` : command} label="复制命令与输出" /></div>
    <div className="terminal-command"><span aria-hidden="true">$</span><code>{command}</code></div>
    {output !== '' && <pre>{output}</pre>}
  </div>
}
