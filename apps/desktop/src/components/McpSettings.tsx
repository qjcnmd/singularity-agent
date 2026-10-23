import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react'
import { appStore } from '../appStore'
import type { McpConnectionStatus, McpServerInput, McpServerState } from '../protocol'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

const connectionLabels: Record<McpConnectionStatus, string> = {
  not_connected: '尚未连接', connecting: '连接中…', connected: '已连接', failed: '连接失败', disabled: '已关闭',
}

export function McpSettings({ workspaceId, active, busy, onBusyChange }: {
  workspaceId: string | null; active: boolean; busy: boolean; onBusyChange: (busy: boolean) => void
}) {
  const [servers, setServers] = useState<McpServerState[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<string | null>(null)
  const [editor, setEditor] = useState<McpServerInput | 'new' | null>(null)
  const [removing, setRemoving] = useState<McpServerInput | null>(null)
  const revision = useRef(0)

  const load = useCallback(async () => {
    const version = ++revision.current
    try {
      const servers = await appStore.transport.rpc('mcp.list', { workspaceId })
      if (version === revision.current) setServers(servers)
    } catch (error) {
      if (version === revision.current) {
        setError(error instanceof Error ? error.message : '读取 MCP 配置失败。')
      }
    } finally { if (version === revision.current) setLoading(false) }
  }, [workspaceId])

  useEffect(() => {
    if (!active) return
    setLoading(true)
    setError(null)
    void load()
    return () => { revision.current += 1 }
    // 进入 MCP 分类只读取状态；保持挂载让分类切换保留编辑草稿。
  }, [active, load])

  useEffect(() => {
    if (!active || busy || loading || error) return
    // 页面可见时持续读取后端状态；上次读取结束后再计时，不启动服务器。
    const timer = window.setTimeout(() => {
      void load()
    }, 1000)
    return () => window.clearTimeout(timer)
  }, [active, busy, loading, error, servers, load])

  async function reconnect(serverId: string) {
    const version = ++revision.current
    setServers(current => current.map(state => state.server.serverId === serverId
      ? { ...state, status: 'connecting', error: null, tools: [] } : state))
    try {
      const result = await appStore.transport.rpc('mcp.reconnect', { serverId, workspaceId })
      if (revision.current === version) setServers(current => current.map(state => state.server.serverId === serverId ? result : state))
    } catch (error) {
      if (revision.current === version) setServers(current => current.map(state => state.server.serverId === serverId
        ? { ...state, status: 'failed', error: error instanceof Error ? error.message : '连接 MCP 失败。', tools: [] } : state))
    }
  }

  async function mutate(action: () => Promise<McpServerState[]>) {
    if (busy) return false
    onBusyChange(true)
    setError(null)
    const version = ++revision.current
    try {
      const servers = await action()
      if (version !== revision.current) return false
      setServers(servers)
      setLoading(false)
      return true
    }
    catch (error) {
      if (version === revision.current) setError(error instanceof Error ? error.message : '保存 MCP 配置失败。')
      return false
    }
    finally { onBusyChange(false) }
  }

  async function retryLoad() {
    if (busy) return
    onBusyChange(true)
    setLoading(true)
    setError(null)
    try {
      await load()
    } finally { onBusyChange(false) }
  }

  return <section className="mcp-settings" aria-labelledby="mcp-settings-title" aria-busy={loading}>
    <header className="sg-view-header">
      <h3 id="mcp-settings-title">MCP</h3>
      <p>执行任务时自动连接，连接配置和开关在下次运行时更新。每轮运行读取当前指令和工具清单，轮内保持一致。</p>
    </header>
    {error && editor === null && removing === null && <p role="alert" className="form-error">{error} <button type="button" className="quiet-button" disabled={busy || loading} onClick={() => void retryLoad()}>重新读取</button></p>}
    <div className="sg-provider-list">
      {servers.map(result => {
        const server = result.server
        const open = expanded === server.serverId
        return <div className="sg-row-card" key={server.serverId}>
          <div className="sg-row-head mcp-row-head">
            <button type="button" className="sg-provider-toggle" aria-expanded={open} aria-controls={`mcp-tools-${server.serverId}`} onClick={() => setExpanded(open ? null : server.serverId)}>
              <span className="sg-row-identity"><strong>{server.serverId}</strong><span className="mcp-status">{result.status === 'connected' ? `${result.tools.length} 个工具` : connectionLabels[result.status]}</span></span>
              <ExpandChevron expanded={open} size={16} />
            </button>
            <span className="sg-row-actions">
              <label className="mcp-switch"><input type="checkbox" role="switch" aria-label={`启用 MCP ${server.serverId}`} checked={server.enabled} disabled={busy || loading} onChange={event => { const enabled = event.target.checked; void mutate(() => appStore.transport.rpc('mcp.toggle', { serverId: server.serverId, enabled, workspaceId })) }} />启用</label>
              <button type="button" className="quiet-button" disabled={busy || loading} aria-label={`编辑 MCP ${server.serverId}`} onClick={() => { setError(null); setEditor(server) }}>编辑</button>
              <button type="button" className="quiet-button danger" disabled={busy || loading} aria-label={`删除 MCP ${server.serverId}`} onClick={() => { setError(null); setRemoving(server) }}>删除</button>
            </span>
          </div>
          <Disclosure open={open}><div id={`mcp-tools-${server.serverId}`} className="mcp-tools">
            <p className="mcp-endpoint">{server.transport.type === 'stdio' ? `${server.transport.command} ${server.transport.args.join(' ')}` : server.transport.url}</p>
            {result.error && server.enabled && <p role="alert" className="form-error">{result.error}</p>}
            {server.enabled && result.status === 'failed' && <button type="button" className="secondary-button" disabled={busy || loading} onClick={() => void reconnect(server.serverId)}>重试连接</button>}
            {result.tools.map(tool => <div key={tool.name} className="mcp-tool"><strong>{tool.name}</strong><p>{tool.description}</p></div>)}
          </div></Disclosure>
        </div>
      })}
      {!loading && !error && servers.length === 0 && <p className="sg-provider-empty">尚未配置 MCP 服务器。</p>}
    </div>
    <div className="sg-add-actions"><button type="button" className="sg-add-card-btn" disabled={busy || loading} onClick={() => { setError(null); setEditor('new') }}>＋ 添加 MCP</button></div>
    <Dialog open={editor !== null} onClose={() => { if (!busy) setEditor(null) }} labelledBy="mcp-editor-title">
      <header className="modal-header"><h2 id="mcp-editor-title">{editor === 'new' ? '添加 MCP' : '编辑 MCP'}</h2><button type="button" className="quiet-button" disabled={busy} onClick={() => setEditor(null)}>取消</button></header>
      {editor !== null && <McpEditor key={editor === 'new' ? 'new' : editor.serverId} server={editor === 'new' ? undefined : editor} busy={busy} error={error} onSave={async server => {
        if (editor === 'new' && servers.some(current => current.server.serverId === server.serverId)) throw new Error('该 MCP 名称已存在，请编辑已有服务器。')
        if (await mutate(() => appStore.transport.rpc('mcp.save', { server, workspaceId }))) setEditor(null)
      }} />}
    </Dialog>
    <Dialog open={removing !== null} onClose={() => { if (!busy) setRemoving(null) }} labelledBy="mcp-remove-title" className="confirm-modal">
      <header className="modal-header"><h2 id="mcp-remove-title">删除 MCP</h2></header>
      <div className="confirm-body"><p>删除“{removing?.serverId}”的配置？</p>{error && <p role="alert" className="form-error">{error}</p>}<footer>
        <button type="button" className="secondary-button" disabled={busy} onClick={() => setRemoving(null)}>取消</button>
        <button type="button" className="danger-button" disabled={busy} onClick={() => {
          if (removing) void mutate(() => appStore.transport.rpc('mcp.remove', { serverId: removing.serverId, workspaceId })).then(saved => { if (saved) setRemoving(null) })
        }}>删除</button>
      </footer></div>
    </Dialog>
  </section>
}

function McpEditor({ server, busy, error, onSave }: { server?: McpServerInput; busy: boolean; error: string | null; onSave: (server: McpServerInput) => Promise<void> }) {
  const [id, setId] = useState(server?.serverId ?? '')
  const [type, setType] = useState(server?.transport.type ?? 'stdio')
  const stdio = server?.transport.type === 'stdio' ? server.transport : null
  const http = server?.transport.type === 'http' ? server.transport : null
  const [command, setCommand] = useState(stdio?.command ?? '')
  const [args, setArgs] = useState(stdio?.args.join('\n') ?? '')
  const [cwd, setCwd] = useState(stdio?.cwd ?? '')
  const [env, setEnv] = useState(JSON.stringify(stdio?.env ?? {}, null, 2))
  const [url, setUrl] = useState(http?.url ?? '')
  const [headers, setHeaders] = useState(JSON.stringify(http?.headers ?? {}, null, 2))
  const [startup, setStartup] = useState(server?.startupTimeoutSec ?? 30)
  const [timeout, setTimeout] = useState(server?.toolTimeoutSec ?? 120)
  const [failure, setFailure] = useState<string | null>(null)

  async function save(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    setFailure(null)
    try {
      await onSave({ serverId: id.trim(), enabled: server?.enabled ?? true, startupTimeoutSec: startup, toolTimeoutSec: timeout,
        transport: type === 'stdio' ? { type, command: command.trim(), args: args === '' ? [] : args.split('\n'), cwd: cwd.trim() || null, env: stringMap(env, '环境变量') }
          : { type, url: url.trim(), headers: stringMap(headers, '请求头') },
      })
    } catch (error) { setFailure(error instanceof Error ? error.message : '配置格式无效。') }
  }

  return <form className="sg-editor mcp-editor" onSubmit={event => void save(event)}>
    <fieldset className="mcp-editor-scroll" disabled={busy}>
      <label className="sg-field"><span>名称</span><input className="sg-input" aria-label="MCP 名称" required pattern="[A-Za-z0-9_-]+" disabled={!!server} value={id} onChange={event => setId(event.target.value)} placeholder="chrome_devtools" /></label>
      <label className="sg-field"><span>连接方式</span><select className="sg-input" aria-label="MCP 连接方式" value={type} onChange={event => setType(event.target.value as typeof type)}><option value="stdio">本地进程（stdio）</option><option value="http">远程服务（Streamable HTTP）</option></select></label>
      {type === 'stdio' ? <>
        <label className="sg-field"><span>命令</span><input className="sg-input" aria-label="MCP 命令" required value={command} onChange={event => setCommand(event.target.value)} placeholder="npx" /></label>
        <label className="sg-field"><span>参数（每行一个）</span><textarea className="sg-input" aria-label="MCP 参数" rows={3} value={args} onChange={event => setArgs(event.target.value)} placeholder={'-y\nchrome-devtools-mcp@latest'} /></label>
        <label className="sg-field"><span>工作目录（留空使用任务的项目目录）</span><input className="sg-input" aria-label="MCP 工作目录" value={cwd} onChange={event => setCwd(event.target.value)} /></label>
        <label className="sg-field"><span>环境变量（JSON 对象）</span><textarea className="sg-input" aria-label="MCP 环境变量" rows={3} spellCheck={false} value={env} onChange={event => setEnv(event.target.value)} /></label>
      </> : <>
        <label className="sg-field"><span>服务地址</span><input className="sg-input" aria-label="MCP 服务地址" type="url" required value={url} onChange={event => setUrl(event.target.value)} placeholder="https://example.com/mcp" /></label>
        <label className="sg-field"><span>请求头（JSON 对象）</span><textarea className="sg-input" aria-label="MCP 请求头" rows={3} spellCheck={false} value={headers} onChange={event => setHeaders(event.target.value)} placeholder={'{"Authorization": "Bearer token"}'} /></label>
      </>}
      <div className="mcp-timeouts">
        <label className="sg-field">
          <span>连接超时（秒）</span>
          <input
            className="sg-input"
            aria-label="MCP 连接超时"
            type="number"
            min={1}
            step={1}
            required
            value={startup}
            onChange={event => setStartup(Number(event.target.value))}
          />
        </label>
        <label className="sg-field">
          <span>调用超时（秒）</span>
          <input
            className="sg-input"
            aria-label="MCP 调用超时"
            type="number"
            min={1}
            step={1}
            required
            value={timeout}
            onChange={event => setTimeout(Number(event.target.value))}
          />
        </label>
      </div>
      {(failure || error) && <p role="alert" className="form-error">{failure || error}</p>}
    </fieldset>
    <footer><button type="submit" className="primary-button" disabled={busy}>{busy ? '保存中…' : '保存 MCP'}</button></footer>
  </form>
}

function stringMap(text: string, label: string): Record<string, string> {
  const parsed: unknown = JSON.parse(text)
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed) || !Object.values(parsed).every(value => typeof value === 'string')) throw new Error(`${label}必须是值为字符串的 JSON 对象。`)
  return parsed as Record<string, string>
}
