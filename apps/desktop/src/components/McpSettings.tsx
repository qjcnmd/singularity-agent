import { Pencil, Plus, Trash2, X } from 'lucide-react'
import { useCallback, useEffect, useRef, useState, type FormEvent } from 'react'
import { appStore } from '../appStore'
import { formatTokenCount } from '../copy'
import type { McpConnectionStatus, McpServerInput, McpServerState } from '../protocol'
import { Dialog } from './Dialog'
import { Disclosure } from './Disclosure'
import { ExpandChevron } from './ExpandChevron'

const connectionLabels: Record<McpConnectionStatus, string> = {
  not_connected: '尚未连接', connecting: '连接中…', connected: '已连接', failed: '连接失败',
}

export function McpSettings({ workspaceRoot }: { workspaceRoot: string | null }) {
  const [busy, setBusy] = useState(false)
  const [reconnecting, setReconnecting] = useState(0)
  const [servers, setServers] = useState<McpServerState[]>([])
  const [loading, setLoading] = useState(true)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [mutationError, setMutationError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<string | null>(null)
  const [editor, setEditor] = useState<McpServerInput | 'new' | null>(null)
  const [removing, setRemoving] = useState<McpServerInput | null>(null)
  const epoch = useRef(0)
  const loadRevision = useRef(0)
  const reconnectRevisions = useRef<Map<string, number>>(new Map())

  const load = useCallback(async () => {
    const ep = epoch.current
    const version = ++loadRevision.current
    try {
      const servers = await appStore.transport.rpc('mcp.list', { workspaceRoot })
      if (ep === epoch.current && version === loadRevision.current) {
        setLoadError(null)
        setServers(servers)
      }
    } catch (error) {
      if (ep === epoch.current && version === loadRevision.current) {
        setLoadError(error instanceof Error ? error.message : '读取 MCP 配置失败。')
      }
    } finally {
      if (ep === epoch.current && version === loadRevision.current) setLoading(false)
    }
  }, [workspaceRoot])

  useEffect(() => {
    setLoading(true)
    setLoadError(null)
    setMutationError(null)
    void load()
    const revisions = reconnectRevisions.current
    return () => {
      epoch.current += 1
      loadRevision.current += 1
      revisions.clear()
    }
  }, [load])

  const hasConnecting = servers.some(server => server.status === 'connecting')
  useEffect(() => {
    if (busy || loading || loadError || reconnecting > 0 || !hasConnecting) return
    const timer = window.setTimeout(() => {
      void load()
    }, 1000)
    return () => window.clearTimeout(timer)
  }, [busy, loading, loadError, reconnecting, hasConnecting, servers, load])

  async function reconnect(serverId: string) {
    const ep = epoch.current
    loadRevision.current += 1
    const version = (reconnectRevisions.current.get(serverId) ?? 0) + 1
    reconnectRevisions.current.set(serverId, version)
    setReconnecting(count => count + 1)
    setServers(current => current.map(state => state.server.serverId === serverId
      ? { ...state, status: 'connecting', error: null, tools: [] } : state))
    try {
      const result = await appStore.transport.rpc('mcp.reconnect', { serverId, workspaceRoot })
      appStore.invalidateContextPreview()
      if (ep === epoch.current && reconnectRevisions.current.get(serverId) === version) {
        setServers(current => current.map(state => state.server.serverId === serverId ? result : state))
      }
    } catch (error) {
      if (ep === epoch.current && reconnectRevisions.current.get(serverId) === version) {
        setServers(current => current.map(state => state.server.serverId === serverId
          ? { ...state, status: 'failed', error: error instanceof Error ? error.message : '连接 MCP 失败。', tools: [] } : state))
      }
    } finally {
      setReconnecting(count => Math.max(0, count - 1))
    }
  }

  async function mutate(action: () => Promise<McpServerState[]>) {
    if (busy) return false
    setBusy(true)
    setMutationError(null)
    const startEpoch = epoch.current
    loadRevision.current += 1
    try {
      const servers = await action()
      appStore.invalidateContextPreview()
      if (startEpoch !== epoch.current) return false
      epoch.current += 1
      loadRevision.current += 1
      reconnectRevisions.current.clear()
      setLoadError(null)
      setServers(servers)
      setLoading(false)
      return true
    }
    catch (error) {
      if (startEpoch === epoch.current) setMutationError(error instanceof Error ? error.message : '保存 MCP 配置失败。')
      return false
    }
    finally { setBusy(false) }
  }

  async function retryLoad() {
    if (busy) return
    setBusy(true)
    setLoading(true)
    setLoadError(null)
    try {
      await load()
    } finally { setBusy(false) }
  }

  const closeEditor = () => { if (!busy) { setMutationError(null); setEditor(null) } }
  const closeRemoving = () => { if (!busy) { setMutationError(null); setRemoving(null) } }

  return <section aria-label="MCP" aria-busy={loading}>
    {loadError && <p role="alert" className="form-error">{loadError} <button type="button" className="quiet-button" disabled={busy || loading} onClick={() => void retryLoad()}>重新读取</button></p>}
    <div className="sg-provider-list">
      {servers.map(result => {
        const server = result.server
        const open = expanded === server.serverId
        return <div className="sg-row-card" key={server.serverId}>
          <div className="sg-row-head mcp-row-head">
            <button type="button" className="sg-provider-toggle" aria-expanded={open} aria-controls={`mcp-tools-${server.serverId}`} onClick={() => setExpanded(open ? null : server.serverId)}>
              <span className="row-card-title">
                <span className="row-card-head-line">
                  <strong>{server.serverId}</strong>
                  <span className="proto-tag">{server.transport.type === 'stdio' ? 'stdio' : 'HTTP'}</span>
                </span>
                <small className="mcp-endpoint-inline">{server.transport.type === 'stdio' ? `${server.transport.command} ${server.transport.args.join(' ')}`.trim() : server.transport.url}</small>
              </span>
              <span className={`mcp-status-pill status-${result.status}`}>
                {result.status === 'connected' ? `${result.tools.length} 个工具 · ≈ ${formatTokenCount(result.tokens).toLowerCase()} tokens` : connectionLabels[result.status]}
              </span>
              <ExpandChevron expanded={open} size={15} />
            </button>
            <span className="sg-row-actions">
              <button type="button" className="icon-button small" disabled={busy || loading} aria-label={`编辑 MCP ${server.serverId}`} title="编辑" onClick={() => { setMutationError(null); setEditor(server) }}><Pencil size={14} strokeWidth={1.6} aria-hidden="true" /></button>
              <button type="button" className="icon-button small danger" disabled={busy || loading} aria-label={`删除 MCP ${server.serverId}`} title="删除" onClick={() => { setMutationError(null); setRemoving(server) }}><Trash2 size={14} strokeWidth={1.6} aria-hidden="true" /></button>
            </span>
          </div>
          <Disclosure open={open}><div id={`mcp-tools-${server.serverId}`} className="mcp-tools">
            {result.error && <p role="alert" className="form-error">{result.error}</p>}
            {result.status === 'failed' && <button type="button" className="secondary-button small" disabled={busy || loading} onClick={() => void reconnect(server.serverId)}>重试连接</button>}
            {result.tools.length > 0 && <div className="mcp-tool-list">
              {result.tools.map(tool => <div key={tool.name} className="mcp-tool"><strong>{tool.name}</strong>{tool.description && <p>{tool.description}</p>}</div>)}
            </div>}
          </div></Disclosure>
        </div>
      })}
      {!loading && !loadError && servers.length === 0 && <p className="sg-provider-empty">尚未配置 MCP 服务器。</p>}
    </div>
    <button type="button" className="sg-add-card-btn" disabled={busy || loading} onClick={() => { setMutationError(null); setEditor('new') }}><Plus size={15} strokeWidth={1.7} aria-hidden="true" />添加 MCP</button>
    <Dialog open={editor !== null} onClose={closeEditor} labelledBy="mcp-editor-title" className="mcp-editor-modal">
      <header className="modal-header"><h2 id="mcp-editor-title">{editor === 'new' ? '添加 MCP' : '编辑 MCP'}</h2><button type="button" className="icon-button" aria-label="关闭" disabled={busy} onClick={closeEditor}><X size={18} /></button></header>
      {editor !== null && <McpEditor key={editor === 'new' ? 'new' : editor.serverId} server={editor === 'new' ? undefined : editor} onCancel={closeEditor} busy={busy} error={mutationError} onSave={async server => {
        if (editor === 'new' && servers.some(current => current.server.serverId === server.serverId)) throw new Error('该 MCP 名称已存在，请编辑已有服务器。')
        if (await mutate(() => appStore.transport.rpc('mcp.save', { server, workspaceRoot }))) {
          setMutationError(null)
          setEditor(null)
        }
      }} />}
    </Dialog>
    <Dialog open={removing !== null} onClose={closeRemoving} labelledBy="mcp-remove-title" className="confirm-modal">
      <header className="modal-header"><h2 id="mcp-remove-title">删除 MCP</h2></header>
      <div className="confirm-body"><p>删除“{removing?.serverId}”的配置？</p>{mutationError && <p role="alert" className="form-error">{mutationError}</p>}<footer>
        <button type="button" className="secondary-button" disabled={busy} onClick={closeRemoving}>取消</button>
        <button type="button" className="danger-button" disabled={busy} onClick={() => {
          if (removing) void mutate(() => appStore.transport.rpc('mcp.remove', { serverId: removing.serverId, workspaceRoot })).then(saved => { if (saved) { setMutationError(null); setRemoving(null) } })
        }}>删除</button>
      </footer></div>
    </Dialog>
  </section>
}

function McpEditor({ server, busy, error, onSave, onCancel }: { server?: McpServerInput; onCancel: () => void; busy: boolean; error: string | null; onSave: (server: McpServerInput) => Promise<void> }) {
  const [id, setId] = useState(server?.serverId ?? '')
  const [type, setType] = useState(server?.transport.type ?? 'stdio')
  const stdio = server?.transport.type === 'stdio' ? server.transport : null
  const http = server?.transport.type === 'http' ? server.transport : null
  const [command, setCommand] = useState(stdio?.command ?? '')
  const [args, setArgs] = useState(JSON.stringify(stdio?.args ?? [], null, 2))
  const [cwd, setCwd] = useState(stdio?.cwd ?? '')
  const [env, setEnv] = useState(JSON.stringify(stdio?.env ?? {}, null, 2))
  const [url, setUrl] = useState(http?.url ?? '')
  const [headers, setHeaders] = useState(JSON.stringify(http?.headers ?? {}, null, 2))
  const [startup, setStartup] = useState(String(server?.startupTimeoutSec ?? 30))
  const [timeout, setTimeout] = useState(String(server?.toolTimeoutSec ?? 120))
  const [failure, setFailure] = useState<string | null>(null)

  async function save(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    setFailure(null)
    try {
      const parsedArgs: unknown = type === 'stdio' ? JSON.parse(args) : []
      if (!Array.isArray(parsedArgs) || !parsedArgs.every((value): value is string => typeof value === 'string')) throw new Error('参数必须是字符串组成的 JSON 数组。')
      const startupTimeoutSec = positiveInteger(startup, '连接超时')
      const toolTimeoutSec = positiveInteger(timeout, '调用超时')
      await onSave({ serverId: id.trim(), startupTimeoutSec, toolTimeoutSec,
        transport: type === 'stdio' ? { type, command: command.trim(), args: parsedArgs, cwd: cwd.trim() || null, env: stringMap(env, '环境变量') }
          : { type, url: url.trim(), headers: stringMap(headers, '请求头') },
      })
    } catch (error) { setFailure(error instanceof Error ? error.message : '配置格式无效。') }
  }

  return <form className="sg-editor mcp-editor" onSubmit={event => void save(event)}>
    <fieldset className="mcp-editor-scroll" disabled={busy}>
      <label className="sg-field"><span>名称</span><input className="input" aria-label="MCP 名称" required pattern="[A-Za-z0-9_-]+" disabled={!!server} value={id} onChange={event => setId(event.target.value)} /></label>
      <label className="sg-field"><span>连接方式</span><select className="input" aria-label="MCP 连接方式" value={type} onChange={event => setType(event.target.value as typeof type)}><option value="stdio">本地进程（stdio）</option><option value="http">远程服务（Streamable HTTP）</option></select></label>
      {type === 'stdio' ? <>
        <label className="sg-field"><span>命令</span><input className="input" aria-label="MCP 命令" required value={command} onChange={event => setCommand(event.target.value)} /></label>
        <label className="sg-field"><span>参数（JSON 数组）</span><textarea className="input" aria-label="MCP 参数" rows={3} value={args} onChange={event => setArgs(event.target.value)} /></label>
        <label className="sg-field"><span>工作目录（留空使用任务的项目目录）</span><input className="input" aria-label="MCP 工作目录" value={cwd} onChange={event => setCwd(event.target.value)} /></label>
        <label className="sg-field"><span>环境变量（JSON 对象）</span><textarea className="input" aria-label="MCP 环境变量" rows={3} spellCheck={false} value={env} onChange={event => setEnv(event.target.value)} /></label>
      </> : <>
        <label className="sg-field"><span>服务地址</span><input className="input" aria-label="MCP 服务地址" type="url" required value={url} onChange={event => setUrl(event.target.value)} /></label>
        <label className="sg-field"><span>请求头（JSON 对象）</span><textarea className="input" aria-label="MCP 请求头" rows={3} spellCheck={false} value={headers} onChange={event => setHeaders(event.target.value)} /></label>
      </>}
      <div className="mcp-timeouts">
        <label className="sg-field">
          <span>连接超时（秒）</span>
          <input
            className="input"
            aria-label="MCP 连接超时"
            type="number"
            min={1}
            step={1}
            required
            value={startup}
            onChange={event => setStartup(event.target.value)}
          />
        </label>
        <label className="sg-field">
          <span>调用超时（秒）</span>
          <input
            className="input"
            aria-label="MCP 调用超时"
            type="number"
            min={1}
            step={1}
            required
            value={timeout}
            onChange={event => setTimeout(event.target.value)}
          />
        </label>
      </div>
      {(failure || error) && <p role="alert" className="form-error">{failure || error}</p>}
    </fieldset>
    <footer><button type="button" className="secondary-button" disabled={busy} onClick={onCancel}>取消</button><button type="submit" className="primary-button" disabled={busy}>{busy ? '保存中…' : '保存 MCP'}</button></footer>
  </form>
}

function positiveInteger(raw: string, label: string): number {
  const value = Number(raw.trim())
  if (!Number.isInteger(value) || value < 1) throw new Error(`${label}必须是大于 0 的整数。`)
  return value
}

function stringMap(text: string, label: string): Record<string, string> {
  const parsed: unknown = JSON.parse(text)
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed) || !Object.values(parsed).every(value => typeof value === 'string')) throw new Error(`${label}必须是值为字符串的 JSON 对象。`)
  return parsed as Record<string, string>
}
